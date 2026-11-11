//! Listing import at a tag (CN-4.5, `design/connectors.md` §9.1, D23–D25): the one function that
//! writes `marketplace`-tier catalog rows, for `POST /v1/marketplaces/{id}/listings/{listing}/install`,
//! `…/update` and `vam catalog import`.
//!
//! **What an import does**, in this order, and refuses at the first step that fails (nothing is
//! written before the last step):
//!
//! 1. The marketplace is enabled and the listing is in its last good read, without an error. A
//!    listing that names a connector must ship it, unless it comes from the preconfigured Squrell
//!    marketplace, whose listings name a connector built into the node (M4, changed by CN-4.11);
//!    the node must satisfy `requiresNode`.
//! 2. The tag is looked up on the remote again and must still name the commit it is pinned to: a
//!    moved tag is refused, never followed (M5).
//! 3. Every artifact is exported from **that commit**, regular files only, and run through the
//!    shared package check (`vam_spec::package::check_dir`). Its URN must be under the
//!    marketplace's own `publisherDomain` and, for an added marketplace, that domain must not be
//!    one the node reserves (M3). A connector file is held to the limits of a marketplace
//!    connector (`vam_spec::connector_listing`): remote MCP with discovery, no secret, no
//!    Squrell id.
//! 4. Refs inside the listing are frozen to the listing's own artifacts (version and digest); a
//!    ref to anything the listing does not ship is refused. An agent must carry `permissions`.
//! 5. One transaction writes the released rows (connectors, skills, then playbooks, then agents)
//!    with owner `marketplace:<id>`, tier `marketplace`, their origin and the install record. A
//!    failing artifact leaves no row. A connector that is new, or whose capability summary differs
//!    from the installed one (publisher, client id, scopes, MCP server or tools), is set back to
//!    **not enabled** in the same transaction and its members' connections are marked
//!    `needs_reauth`: the admin enables it again, the members connect again.
//!
//! A release already installed with the same content is kept (its origin follows the new tag); a
//! changed agent or playbook gets the next patch version; a changed skill must carry a new
//! version in its manifest. Older releases stay, so an agent pinned to them still resolves.

use prost::Message;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::path::Path;
use vam_spec::connector_listing::{connector_id_of_urn, is_reserved_connector_id};
use vam_spec::marketplace::{urn_domain, NodeRequirement};
use vam_spec::package::{check_dir, ArtifactKind, CheckedPackage, CheckedSpec};
use vam_spec::v1::{AgentSpec, Digest, PlaybookSpec, ResourceRef, SkillManifest, Version};

use super::installs::{self, InstallRow, InstalledArtifact, OriginRow};
use super::store::{ListingState, MarketplaceRow, Trust};
use super::{cached_tag_commit, CachedTagError, MarketplaceConfig};
use crate::catalog_tier::CATALOG_TIER_MARKETPLACE;
use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

const SHA256: i32 = 1;

/// How an import was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The listing is not installed yet.
    Install,
    /// The listing is installed and the index now names another tag.
    Update,
}

/// What an import found or wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub marketplace_id: String,
    pub listing_id: String,
    pub git_ref: String,
    pub commit: String,
    /// False when the listing was already installed at this tag and nothing changed.
    pub changed: bool,
    pub artifacts: Vec<InstalledArtifact>,
    /// URNs of artifacts that were installed before and whose capability summary is now
    /// different (an update that changes what a tool can reach, an agent's rules, ...). The
    /// connector kind uses this to ask for Enable again.
    pub capabilities_changed: Vec<String>,
}

fn invalid(m: impl Into<String>) -> OrchestratorError {
    OrchestratorError::InvalidContract(m.into())
}

fn conflict(m: impl Into<String>) -> OrchestratorError {
    OrchestratorError::Conflict(m.into())
}

fn now() -> i64 {
    crate::connectors::now_secs()
}

/// Installs or updates `listing_id` of marketplace `marketplace_id` at the tag the last read of the
/// index named. `by` is the caller's user id. `want_ref` is the tag a client believes it is
/// installing (the CLI's `--ref`); when given it must equal the index's. Blocking: run it on a
/// blocking thread, under [`super::lock_marketplace`].
pub fn install_blocking(
    store: &SqliteWorkflowStore,
    cfg: &MarketplaceConfig,
    marketplace_id: &str,
    listing_id: &str,
    mode: Mode,
    want_ref: Option<&str>,
    by: &str,
) -> Result<Outcome, OrchestratorError> {
    let row = store
        .marketplace(marketplace_id)?
        .ok_or_else(|| OrchestratorError::NotFound(format!("marketplace `{marketplace_id}`")))?;
    if !row.enabled {
        return Err(conflict(format!(
            "marketplace `{}` is disabled; enable it first",
            row.name
        )));
    }
    let listing = row
        .listings
        .iter()
        .find(|l| l.id == listing_id)
        .ok_or_else(|| {
            OrchestratorError::NotFound(format!(
                "listing `{listing_id}` of marketplace `{}`",
                row.name
            ))
        })?
        .clone();
    if let Some(want) = want_ref.filter(|w| *w != listing.git_ref) {
        return Err(conflict(format!(
            "listing `{listing_id}` is at `{}` in the index this node last read, not `{want}`; refresh \
             the marketplace and install the tag it names",
            listing.git_ref
        )));
    }

    // M4 (changed by CN-4.11): a listing from any marketplace may carry a connector, within the
    // limits checked per file in `prepare`. A listing from an Unverified marketplace may name only a
    // connector it ships itself; a name that points elsewhere (a built-in connector, another
    // listing's) is refused. Only the Squrell marketplace names the connectors built into the node.
    let squrell = row.preconfigured && row.trust == Trust::Squrell;
    if let Some(connector) = &listing.connector {
        let ships_it = listing
            .artifacts
            .iter()
            .any(|a| a.kind == ArtifactKind::Connector.as_str() && &a.urn == connector);
        if !squrell && !ships_it {
            return Err(invalid(format!(
                "listing `{listing_id}` names the connector {connector} but does not ship it: a \
                 listing from an Unverified marketplace may name only a connector it ships itself \
                 (an artifact of kind connector)"
            )));
        }
    }
    if let Some(why) = &listing.error {
        return Err(conflict(format!(
            "listing `{listing_id}` cannot be installed: {why}"
        )));
    }
    match NodeRequirement::parse(&listing.requires_node) {
        Ok(req) if req.matches(&cfg.node_version) => {}
        Ok(_) => {
            return Err(invalid(format!(
                "listing `{listing_id}` requires node {}; this node is {}",
                listing.requires_node, cfg.node_version
            )))
        }
        Err(why) => return Err(invalid(format!("requiresNode {why}"))),
    }

    // M5: the commit the tag is pinned to, checked against the remote now.
    let commit = cached_tag_commit(store, cfg, &row, &listing.git_ref).map_err(tag_error)?;

    let current = store.marketplace_install(marketplace_id, listing_id)?;
    match (&current, mode) {
        (Some(cur), _) if cur.git_ref == listing.git_ref && cur.commit == commit => {
            return Ok(Outcome {
                marketplace_id: marketplace_id.to_string(),
                listing_id: listing_id.to_string(),
                git_ref: cur.git_ref.clone(),
                commit,
                changed: false,
                artifacts: cur.artifacts.clone(),
                capabilities_changed: Vec::new(),
            });
        }
        (Some(cur), Mode::Install) => {
            return Err(conflict(format!(
                "listing `{listing_id}` is installed at `{}`; the index names `{}`: update it instead",
                cur.git_ref, listing.git_ref
            )));
        }
        (None, Mode::Update) => {
            return Err(conflict(format!(
                "listing `{listing_id}` is not installed; install it first"
            )));
        }
        _ => {}
    }

    let all = store.marketplaces()?;
    let others: Vec<&MarketplaceRow> = all.iter().filter(|m| m.id != row.id).collect();
    let prepared = prepare(cfg, &row, &listing, &commit, &others)?;
    let previous: &[InstalledArtifact] = current.as_ref().map_or(&[], |c| &c.artifacts);
    let written = write(store, &row, &listing, &commit, prepared, previous, by)?;
    // What an update changed in what the artifacts can do, against the install it replaces.
    let capabilities_changed = current
        .iter()
        .flat_map(|cur| cur.artifacts.iter())
        .filter(|old| !old.summary_digest.is_empty())
        .filter(|old| {
            written
                .iter()
                .any(|new| new.urn == old.urn && new.summary_digest != old.summary_digest)
        })
        .map(|old| old.urn.clone())
        .collect();
    Ok(Outcome {
        marketplace_id: marketplace_id.to_string(),
        listing_id: listing_id.to_string(),
        git_ref: listing.git_ref,
        commit,
        changed: true,
        artifacts: written,
        capabilities_changed,
    })
}

fn tag_error(e: CachedTagError) -> OrchestratorError {
    match e {
        CachedTagError::Unpinned(_) | CachedTagError::Gone(_) | CachedTagError::Moved(_) => {
            conflict(e.to_string())
        }
        other => invalid(format!("could not read the marketplace: {other}")),
    }
}

// ---- step 3: read and check what the tag holds ---------------------------------------------------

/// One artifact, exported from the pinned commit and checked.
struct Prepared {
    kind: ArtifactKind,
    path: String,
    checked: CheckedPackage,
    /// Skills: the package archive and its digest (`vam_storage::skills_digest_from_dir`).
    skill: Option<SkillPackage>,
}

struct SkillPackage {
    archive: Vec<u8>,
    digest: String,
}

fn prepare(
    cfg: &MarketplaceConfig,
    row: &MarketplaceRow,
    listing: &ListingState,
    commit: &str,
    others: &[&MarketplaceRow],
) -> Result<Vec<Prepared>, OrchestratorError> {
    let f = &cfg.fetcher;
    let domains = crate::registry::registry_domains();
    if !row.preconfigured {
        if let Some(why) = super::reserved_domain_reason(&row.publisher_domain, &domains, others) {
            return Err(invalid(format!(
                "marketplace `{}` may not publish under `{}`: {why}",
                row.name, row.publisher_domain
            )));
        }
    }
    if listing.artifacts.is_empty() {
        return Err(invalid(format!(
            "listing `{}` has no artifacts to install",
            listing.id
        )));
    }
    let mut out = Vec::new();
    for a in &listing.artifacts {
        let kind = ArtifactKind::parse(&a.kind)
            .ok_or_else(|| invalid(format!("unknown artifact kind `{}`", a.kind)))?;
        let fail =
            |why: String| invalid(format!("{} `{}`: {}", kind.as_str(), a.path, f.scrub(&why)));
        let scratch = f.scratch(&row.id).map_err(|e| fail(e.to_string()))?;
        f.export_dir(&row.id, commit, &a.path, scratch.path())
            .map_err(|e| fail(e.to_string()))?;
        let checked = check_dir(scratch.path(), Some(kind)).map_err(|e| fail(e.0.clone()))?;

        // M3: where the URN lives.
        let urn_dom = urn_domain(&checked.urn).unwrap_or_default().to_string();
        if urn_dom != row.publisher_domain {
            return Err(fail(format!(
                "the artifact URN {} is outside the marketplace's publisher domain {}",
                checked.urn, row.publisher_domain
            )));
        }
        if !row.preconfigured {
            if let Some(why) = super::reserved_domain_reason(&urn_dom, &domains, others) {
                return Err(fail(format!(
                    "the artifact URN {} is under a domain this marketplace may not use: {why}",
                    checked.urn
                )));
            }
        }
        let skill = match &checked.spec {
            CheckedSpec::Skill(_) => {
                let digest = vam_storage::skills_digest_from_dir(scratch.path());
                if digest == "none" {
                    return Err(fail("the skill package is empty".into()));
                }
                let archive = pack_tar_gz(scratch.path()).map_err(fail)?;
                if archive.len() > crate::skill_package_admit::MAX_SKILL_PACKAGE_BYTES {
                    return Err(fail("the skill package is too large".into()));
                }
                Some(SkillPackage { archive, digest })
            }
            _ => None,
        };
        out.push(Prepared {
            kind,
            path: a.path.clone(),
            checked,
            skill,
        });
    }
    // One URN is one artifact.
    let mut seen = std::collections::BTreeSet::new();
    for p in &out {
        if !seen.insert(p.checked.urn.clone()) {
            return Err(invalid(format!(
                "listing `{}` names {} twice",
                listing.id, p.checked.urn
            )));
        }
    }
    Ok(out)
}

/// A deterministic `.tar.gz` of the regular files under `dir` (sorted, no timestamps, no owners),
/// with the files at the archive root, the layout of an uploaded skill package.
fn pack_tar_gz(dir: &Path) -> Result<Vec<u8>, String> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::os::unix::fs::PermissionsExt;

    fn walk(
        base: &Path,
        cur: &Path,
        out: &mut Vec<(String, std::path::PathBuf, u32)>,
    ) -> Result<(), String> {
        for e in std::fs::read_dir(cur).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            let ft = e.file_type().map_err(|e| e.to_string())?;
            let path = e.path();
            if ft.is_dir() {
                walk(base, &path, out)?;
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                let mode = e
                    .metadata()
                    .map_err(|e| e.to_string())?
                    .permissions()
                    .mode();
                out.push((rel, path, if mode & 0o111 != 0 { 0o755 } else { 0o644 }));
            } else {
                return Err(format!("{} is not a regular file", path.display()));
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    walk(dir, dir, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        for (rel, path, mode) in &files {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(*mode);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();
            builder
                .append_data(&mut header, rel, &bytes[..])
                .map_err(|e| e.to_string())?;
        }
        builder.finish().map_err(|e| e.to_string())?;
    }
    let mut gz = Vec::new();
    {
        let mut enc = GzEncoder::new(&mut gz, Compression::default());
        std::io::Write::write_all(&mut enc, &tar_buf).map_err(|e| e.to_string())?;
        enc.finish().map_err(|e| e.to_string())?;
    }
    Ok(gz)
}

// ---- step 4: refs stay inside the listing ----------------------------------------------------------

/// What the listing ships, by URN: the release it is pinned to and the digest recorded for it.
type Pins = HashMap<String, (Version, String)>;

fn release_version((major, minor, patch): (i32, i32, i32)) -> Version {
    Version {
        major,
        minor,
        patch,
        prerelease: String::new(),
        build: String::new(),
    }
}

/// Pins `r` to the listing's own artifact `known` names, or refuses.
fn pin_ref(r: &mut ResourceRef, known: &Pins, owner_urn: &str, what: &str) -> Result<(), String> {
    let urn = r
        .resource_id
        .as_ref()
        .map(|id| id.urn.clone())
        .unwrap_or_default();
    if urn.is_empty() {
        return Ok(());
    }
    let Some((version, digest)) = known.get(&urn) else {
        return Err(format!(
            "{owner_urn} refers to {urn} ({what}), which this listing does not ship: a marketplace \
             listing's artifacts may refer only to each other"
        ));
    };
    if let Some(v) = &r.version {
        if !v.prerelease.is_empty()
            || (v.major, v.minor, v.patch) != (version.major, version.minor, version.patch)
        {
            return Err(format!(
                "{owner_urn} pins {urn} {}.{}.{}{} but this listing ships {}.{}.{}",
                v.major,
                v.minor,
                v.patch,
                if v.prerelease.is_empty() {
                    String::new()
                } else {
                    format!("-{}", v.prerelease)
                },
                version.major,
                version.minor,
                version.patch
            ));
        }
    }
    if let Some(d) = &r.digest {
        if !crate::catalog_resolve::is_unpinned_digest(&d.hex_hash) && &d.hex_hash != digest {
            return Err(format!(
                "{owner_urn} pins {urn} with digest {} but this listing ships {digest}",
                d.hex_hash
            ));
        }
    }
    *r = ResourceRef {
        resource_id: r.resource_id.take(),
        version: Some(version.clone()),
        digest: Some(Digest {
            algorithm: SHA256,
            hex_hash: digest.clone(),
        }),
        locators: std::mem::take(&mut r.locators),
    };
    Ok(())
}

fn freeze_playbook(spec: &mut PlaybookSpec, skills: &Pins) -> Result<(), String> {
    let urn = spec
        .resource_id
        .as_ref()
        .map(|r| r.urn.clone())
        .unwrap_or_default();
    if let Some(requires) = spec.requires.as_mut() {
        for r in &mut requires.skills {
            pin_ref(r, skills, &urn, "a required skill")?;
        }
    }
    Ok(())
}

fn freeze_agent(spec: &mut AgentSpec, skills: &Pins, playbooks: &Pins) -> Result<(), String> {
    let urn = spec
        .resource_id
        .as_ref()
        .map(|r| r.urn.clone())
        .unwrap_or_default();
    // Installed agents run in permissions mode (the legacy policies would name artifacts a
    // marketplace cannot ship).
    if spec.permissions.is_none() {
        return Err(format!(
            "{urn} has no `permissions`: an installed agent must declare its permissions"
        ));
    }
    let none = Pins::new();
    for r in &mut spec.skill_refs {
        pin_ref(r, skills, &urn, "a skill")?;
    }
    if let Some(skills_overlay) = spec.overlay.as_mut().and_then(|o| o.skills.as_mut()) {
        for r in &mut skills_overlay.add {
            pin_ref(r, skills, &urn, "an overlay skill")?;
        }
    }
    if let Some(r) = spec.default_playbook_ref.as_mut() {
        pin_ref(r, playbooks, &urn, "the default playbook")?;
    }
    for r in &mut spec.allowed_playbook_refs {
        pin_ref(r, playbooks, &urn, "an allowed playbook")?;
    }
    // Policies, evaluators and `extends` cannot be shipped by a listing.
    if let Some(r) = spec
        .capability_policy
        .as_mut()
        .and_then(|p| p.policy_ref.as_mut())
    {
        pin_ref(r, &none, &urn, "the capability policy")?;
    }
    if let Some(r) = spec
        .hitl_policy
        .as_mut()
        .and_then(|p| p.policy_ref.as_mut())
    {
        pin_ref(r, &none, &urn, "the approval policy")?;
    }
    if let Some(r) = spec
        .telemetry_policy
        .as_mut()
        .and_then(|p| p.policy_ref.as_mut())
    {
        pin_ref(r, &none, &urn, "the telemetry policy")?;
    }
    if let Some(c) = spec.completion_policy.as_mut() {
        if let Some(r) = c.policy_ref.as_mut() {
            pin_ref(r, &none, &urn, "the completion policy")?;
        }
        for r in &mut c.evaluator_refs {
            pin_ref(r, &none, &urn, "an evaluator")?;
        }
    }
    if let Some(r) = spec.extends.as_mut() {
        pin_ref(r, &none, &urn, "the agent it extends")?;
    }
    Ok(())
}

fn encode<M: Message>(m: &M) -> Result<(Vec<u8>, String), OrchestratorError> {
    let mut bytes = Vec::new();
    m.encode(&mut bytes)
        .map_err(|e| OrchestratorError::Core(e.to_string()))?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    Ok((bytes, digest))
}

// ---- step 5: one transaction ---------------------------------------------------------------------

/// A row the transaction writes, or finds already there.
struct Planned {
    kind: ArtifactKind,
    table: &'static str,
    urn: String,
    version: (i32, i32, i32),
    digest: String,
    /// Digest of the artifact's capability summary (see [`summary_digest`]).
    summary_digest: String,
    /// What a new row stores; `None` for a release that is already installed unchanged.
    row: Option<NewRow>,
}

struct NewRow {
    spec_bytes: Vec<u8>,
    /// Skills: `(blob_relpath, size_bytes)`.
    blob: Option<(String, i64)>,
}

fn table_of(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Skill => "skill_packages",
        ArtifactKind::Agent => "agents",
        ArtifactKind::Playbook => "playbooks",
        ArtifactKind::Connector => "connector_releases",
    }
}

struct Latest {
    version: (i32, i32, i32),
    digest: String,
}

/// The newest release of `urn`, and a refusal when the URN is not wholly this listing's.
fn existing(
    tx: &Connection,
    row: &MarketplaceRow,
    listing: &ListingState,
    kind: ArtifactKind,
    table: &str,
    urn: &str,
) -> Result<Option<Latest>, OrchestratorError> {
    // Any row of the URN that is not an install (a draft, an org release) is the org's: refuse.
    let mut tables = vec![table];
    if kind == ArtifactKind::Skill {
        tables.push("skills");
    }
    for t in tables {
        let foreign: Option<i64> = tx
            .query_row(
                &format!("SELECT 1 FROM {t} WHERE urn = ?1 AND catalog_tier != ?2 LIMIT 1"),
                params![urn, CATALOG_TIER_MARKETPLACE],
                |r| r.get(0),
            )
            .optional()?;
        if foreign.is_some() {
            return Err(conflict(format!(
                "{} `{urn}` already exists in this org's catalog; a marketplace cannot install over it",
                kind.as_str()
            )));
        }
    }
    // The URN must be this listing's, not another listing's.
    let other: Option<(String, String)> = tx
        .query_row(
            "SELECT marketplace_id, listing_id FROM catalog_origins
             WHERE kind = ?1 AND urn = ?2 AND NOT (marketplace_id = ?3 AND listing_id = ?4) LIMIT 1",
            params![kind.as_str(), urn, row.id, listing.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((m, l)) = other {
        return Err(conflict(format!(
            "{} `{urn}` is already installed by listing `{l}` of marketplace `{m}`",
            kind.as_str()
        )));
    }
    Ok(tx
        .query_row(
            &format!(
                "SELECT major, minor, patch, digest FROM {table}
                 WHERE urn = ?1 AND channel = 'released'
                 ORDER BY major DESC, minor DESC, patch DESC LIMIT 1"
            ),
            params![urn],
            |r| {
                Ok(Latest {
                    version: (r.get(0)?, r.get(1)?, r.get(2)?),
                    digest: r.get(3)?,
                })
            },
        )
        .optional()?)
}

fn row_at(
    tx: &Connection,
    table: &str,
    urn: &str,
    v: (i32, i32, i32),
) -> Result<Option<String>, OrchestratorError> {
    Ok(tx
        .query_row(
            &format!(
                "SELECT digest FROM {table}
                 WHERE urn = ?1 AND major = ?2 AND minor = ?3 AND patch = ?4 AND channel = 'released'"
            ),
            params![urn, v.0, v.1, v.2],
            |r| r.get(0),
        )
        .optional()?)
}

/// The order artifacts are planned and written in: what others refer to comes first.
const IMPORT_ORDER: [ArtifactKind; 4] = [
    ArtifactKind::Connector,
    ArtifactKind::Skill,
    ArtifactKind::Playbook,
    ArtifactKind::Agent,
];

/// What planning one artifact reads: the open transaction, whose listing it is, and the releases
/// planned so far (the refs inside a listing may name only these).
struct PlanCtx<'a> {
    tx: &'a Connection,
    row: &'a MarketplaceRow,
    listing: &'a ListingState,
    owner: &'a str,
    skills: Pins,
    playbooks: Pins,
}

/// SHA-256 of the artifact's capability summary, in words an admin reviewed before installing.
/// An update whose summary digest differs from the installed one changed what the artifact can do.
/// Wording (display names and descriptions) is left out: rewording is not a change of capability.
fn summary_digest(p: &Prepared) -> String {
    fn strip(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(m) => {
                m.remove("displayName");
                m.remove("description");
                m.values_mut().for_each(strip);
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut v = serde_json::to_value(&p.checked.summary).unwrap_or_default();
    strip(&mut v);
    format!("{:x}", Sha256::digest(v.to_string().as_bytes()))
}

/// Decides the release one artifact is written as: the frozen content, its digest, and whether it
/// is a new row or a release that is already installed unchanged.
fn plan_artifact(ctx: &mut PlanCtx<'_>, p: &Prepared) -> Result<Planned, OrchestratorError> {
    let (tx, row, listing) = (ctx.tx, ctx.row, ctx.listing);
    let urn = p.checked.urn.clone();
    let table = table_of(p.kind);
    let summary = summary_digest(p);
    match (&p.checked.spec, &p.skill) {
        // A skill keeps the version its manifest declares; changed content needs a new version.
        (CheckedSpec::Skill(manifest), Some(pkg)) => {
            let v = manifest.version.clone().unwrap_or_default();
            let version = (v.major, v.minor, v.patch);
            let latest = existing(tx, row, listing, p.kind, table, &urn)?;
            let new_row = match row_at(tx, table, &urn, version)? {
                Some(d) if d == pkg.digest => None,
                Some(_) => {
                    return Err(conflict(format!(
                        "skill `{urn}` {}.{}.{} is already installed with different content: the \
                         marketplace must release the changed skill under a new version",
                        version.0, version.1, version.2
                    )))
                }
                None => {
                    if let Some(l) = &latest {
                        if version < l.version {
                            return Err(conflict(format!(
                                "skill `{urn}` {}.{}.{} is older than the installed {}.{}.{}",
                                version.0,
                                version.1,
                                version.2,
                                l.version.0,
                                l.version.1,
                                l.version.2
                            )));
                        }
                    }
                    let mut clean: SkillManifest = manifest.clone();
                    clean.version = Some(release_version(version));
                    let (spec_bytes, _) = encode(&clean)?;
                    Some(NewRow {
                        spec_bytes,
                        blob: Some((
                            crate::skill_packages::skill_blob_relpath(ctx.owner, &pkg.digest),
                            pkg.archive.len() as i64,
                        )),
                    })
                }
            };
            ctx.skills
                .insert(urn.clone(), (release_version(version), pkg.digest.clone()));
            Ok(Planned {
                kind: p.kind,
                table,
                urn,
                version,
                digest: pkg.digest.clone(),
                summary_digest: summary,
                row: new_row,
            })
        }
        // An agent, a playbook or a connector is versioned by the catalog: 1.0.0, then the next
        // patch whenever the content changes. Agents and playbooks are first frozen to the
        // listing's own releases.
        (CheckedSpec::Playbook(_) | CheckedSpec::Agent(_) | CheckedSpec::Connector(_), _) => {
            if let CheckedSpec::Connector(def) = &p.checked.spec {
                admit_connector_id(tx, &urn, def)?;
            }
            let (bytes, digest) = match &p.checked.spec {
                CheckedSpec::Playbook(spec) => {
                    let mut spec = spec.clone();
                    freeze_playbook(&mut spec, &ctx.skills)
                        .map_err(|e| invalid(format!("playbook `{}`: {e}", p.path)))?;
                    encode(&spec)?
                }
                CheckedSpec::Agent(spec) => {
                    let mut spec = spec.clone();
                    freeze_agent(&mut spec, &ctx.skills, &ctx.playbooks)
                        .map_err(|e| invalid(format!("agent `{}`: {e}", p.path)))?;
                    encode(&spec)?
                }
                CheckedSpec::Connector(def) => {
                    let bytes = serde_json::to_vec(&**def)
                        .map_err(|e| OrchestratorError::Core(e.to_string()))?;
                    let digest = format!("{:x}", Sha256::digest(&bytes));
                    (bytes, digest)
                }
                CheckedSpec::Skill(_) => unreachable_spec(p)?,
            };
            let (version, new_row) = match existing(tx, row, listing, p.kind, table, &urn)? {
                Some(l) if l.digest == digest => (l.version, None),
                Some(l) => (
                    (l.version.0, l.version.1, l.version.2.saturating_add(1)),
                    Some(NewRow {
                        spec_bytes: bytes,
                        blob: None,
                    }),
                ),
                None => (
                    (1, 0, 0),
                    Some(NewRow {
                        spec_bytes: bytes,
                        blob: None,
                    }),
                ),
            };
            if p.kind == ArtifactKind::Playbook {
                ctx.playbooks
                    .insert(urn.clone(), (release_version(version), digest.clone()));
            }
            Ok(Planned {
                kind: p.kind,
                table,
                urn,
                version,
                digest,
                summary_digest: summary,
                row: new_row,
            })
        }
        (CheckedSpec::Skill(_), None) => unreachable_spec(p),
    }
}

/// The id of an installed connector belongs to one URN: a connector whose id another installed
/// connector (from any listing or marketplace) already has is refused, and so is a Squrell id.
/// Refused here too, under the lock of the transaction, though the package check already did.
fn admit_connector_id(
    tx: &Connection,
    urn: &str,
    def: &vam_spec::connector::Connector,
) -> Result<(), OrchestratorError> {
    let id = connector_id_of_urn(&def.urn);
    if is_reserved_connector_id(id) {
        return Err(invalid(format!(
            "connector `{urn}`: `{id}` is a Squrell connector id"
        )));
    }
    for other in super::connector_store::installed_urns_in(tx)? {
        if other != urn && connector_id_of_urn(&other) == id {
            return Err(conflict(format!(
                "connector `{urn}`: the id `{id}` is already taken by the installed connector {other}"
            )));
        }
    }
    Ok(())
}

/// A connector the import installs is not enabled, and one whose capability summary changed is
/// set back to not enabled: the admin approved the old summary. Members' connections to it are
/// marked `needs_reauth`, so nothing is sent on the old shape (a member connects again after the
/// admin enabled it again). A connector that was not installed before has no connections.
fn reset_connector(
    tx: &Connection,
    urn: &str,
    new_url: &str,
    had_before: bool,
    by: &str,
    at: i64,
) -> Result<(), OrchestratorError> {
    let id = connector_id_of_urn(urn);
    tx.execute(
        "UPDATE node_enabled_connectors SET enabled = 0, updated_at = ?2, updated_by = ?3
         WHERE connector_id = ?1",
        params![id, at, by],
    )?;
    if had_before {
        // The rows on the server the connector named before and on the one it names now are the
        // members' connections to it; a server added by hand under the same id is not.
        let old_url = super::connector_store::installed_by_id_in(tx, id)?
            .and_then(|i| i.def.mcp.map(|m| m.url))
            .unwrap_or_default();
        crate::mcp_registry::mark_connector_rows_need_reauth(
            tx,
            id,
            &[old_url.as_str(), new_url],
            "the connector changed: reconnect once the admin has enabled it again",
        )?;
    }
    Ok(())
}

fn prepared_by_urn<'a>(prepared: &'a [Prepared], urn: &str) -> Option<&'a Prepared> {
    prepared.iter().find(|p| p.checked.urn == urn)
}

fn unreachable_spec<T>(p: &Prepared) -> Result<T, OrchestratorError> {
    Err(OrchestratorError::CorruptState(format!(
        "{} `{}` was prepared without its content",
        p.kind.as_str(),
        p.path
    )))
}

fn write(
    store: &SqliteWorkflowStore,
    row: &MarketplaceRow,
    listing: &ListingState,
    commit: &str,
    prepared: Vec<Prepared>,
    previous: &[InstalledArtifact],
    by: &str,
) -> Result<Vec<InstalledArtifact>, OrchestratorError> {
    let owner = installs::owner_of(&row.id);
    // The archives go to disk first, content-addressed and idempotent: if the transaction fails
    // they are unreferenced, never wrong.
    for p in &prepared {
        if let Some(s) = &p.skill {
            crate::skill_package_admit::write_content_addressed_blob(
                &crate::skill_packages::skill_blob_relpath(&owner, &s.digest),
                &s.archive,
            )?;
        }
    }

    let mut conn = store
        .connection
        .lock()
        .map_err(|_| OrchestratorError::StorageUnavailable)?;
    let tx = conn.transaction()?;
    let at = now();
    SqliteWorkflowStore::ensure_marketplace_owner(&tx, &row.id, at)?;

    // One generic loop over the artifacts, in dependency order. What differs per kind is one arm
    // of `plan_artifact` (its admission and how its content is frozen and versioned) and one row
    // of `table_of` (where it is written); the origin and the install record are the same for
    // every kind. A kind added later (a connector) is an arm, a table and a place in
    // `IMPORT_ORDER`.
    let mut ctx = PlanCtx {
        tx: &tx,
        row,
        listing,
        owner: &owner,
        skills: Pins::new(),
        playbooks: Pins::new(),
    };
    let mut plan: Vec<Planned> = Vec::new();
    for kind in IMPORT_ORDER {
        for p in prepared.iter().filter(|p| p.kind == kind) {
            plan.push(plan_artifact(&mut ctx, p)?);
        }
    }

    // A connector whose capabilities are not the ones an admin saw is not enabled (D4, D26).
    for pl in plan.iter().filter(|pl| pl.kind == ArtifactKind::Connector) {
        let old = previous.iter().find(|a| a.urn == pl.urn);
        let same = old
            .is_some_and(|o| !o.summary_digest.is_empty() && o.summary_digest == pl.summary_digest);
        if !same {
            let new_url = match &prepared_by_urn(&prepared, &pl.urn).map(|p| &p.checked.spec) {
                Some(CheckedSpec::Connector(def)) => {
                    def.mcp.as_ref().map(|m| m.url.clone()).unwrap_or_default()
                }
                _ => String::new(),
            };
            reset_connector(&tx, &pl.urn, &new_url, old.is_some(), by, at)?;
        }
    }

    // Write.
    let note = format!(
        "installed from marketplace {} (listing {}) at {}",
        row.id, listing.id, listing.git_ref
    );
    let mut artifacts = Vec::new();
    for pl in &plan {
        if let Some(new) = &pl.row {
            insert_release(&tx, pl, new, &owner, &note, by, at)?;
        }
        installs::write_origin(
            &tx,
            pl.kind.as_str(),
            &pl.urn,
            pl.version,
            &OriginRow {
                marketplace: row.id.clone(),
                listing: listing.id.clone(),
                git_ref: listing.git_ref.clone(),
                commit: commit.to_string(),
                package_digest: pl.digest.clone(),
            },
            at,
        )?;
        artifacts.push(InstalledArtifact {
            kind: pl.kind.as_str().to_string(),
            urn: pl.urn.clone(),
            version: format!("{}.{}.{}", pl.version.0, pl.version.1, pl.version.2),
            digest: pl.digest.clone(),
            summary_digest: pl.summary_digest.clone(),
        });
    }
    installs::write_install(
        &tx,
        &InstallRow {
            marketplace_id: row.id.clone(),
            listing_id: listing.id.clone(),
            git_ref: listing.git_ref.clone(),
            commit: commit.to_string(),
            installed_at: at,
            installed_by: by.to_string(),
            artifacts: artifacts.clone(),
        },
    )?;
    tx.commit()?;
    Ok(artifacts)
}

fn insert_release(
    tx: &Connection,
    pl: &Planned,
    new: &NewRow,
    owner: &str,
    note: &str,
    by: &str,
    at: i64,
) -> Result<(), OrchestratorError> {
    let (major, minor, patch) = pl.version;
    match &new.blob {
        Some((relpath, size)) => {
            tx.execute(
                "INSERT INTO skill_packages (
                    urn, major, minor, patch, prerelease, channel, revision, digest,
                    blob_relpath, size_bytes, spec_bytes, created_at, updated_at, owner_user_id,
                    catalog_tier, release_note, released_by
                 ) VALUES (?1, ?2, ?3, ?4, '', 'released', 1, ?5, ?6, ?7, ?8, ?9, ?9, ?10, ?11, ?12, ?13)",
                params![
                    pl.urn,
                    major,
                    minor,
                    patch,
                    pl.digest,
                    relpath,
                    size,
                    new.spec_bytes,
                    at,
                    owner,
                    CATALOG_TIER_MARKETPLACE,
                    note,
                    by
                ],
            )?;
        }
        None => {
            tx.execute(
                &format!(
                    "INSERT INTO {} (
                        urn, major, minor, patch, prerelease, channel, revision, digest,
                        spec_bytes, created_at, updated_at, owner_user_id, catalog_tier,
                        release_note, released_by
                     ) VALUES (?1, ?2, ?3, ?4, '', 'released', 1, ?5, ?6, ?7, ?7, ?8, ?9, ?10, ?11)",
                    pl.table
                ),
                params![
                    pl.urn,
                    major,
                    minor,
                    patch,
                    pl.digest,
                    new.spec_bytes,
                    at,
                    owner,
                    CATALOG_TIER_MARKETPLACE,
                    note,
                    by
                ],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
