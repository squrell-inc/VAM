//! Floating catalog refs (no version) resolve on the host, in one place:
//! [`resolve_ref_for_owner`].
//!
//! Agents stay pinned on [`RunSpec::agent_ref`]. Skill, playbook, and policy
//! refs may omit version; the host records the resolved pin on the run.

use vam_spec::v1::{AgentSpec, Digest, ResourceId, ResourceRef, RunSpec, SkillChannel, Version};
use vam_spec::validation::Validate;

use crate::catalog::CatalogPin;
use crate::error::OrchestratorError;
use crate::skill_package_resolve::EMPTY_SHA256_HEX;
use crate::workflow::SqliteWorkflowStore;

const SHA256: i32 = 1;

pub fn is_unversioned(resource_ref: &ResourceRef) -> bool {
    resource_ref.version.is_none()
}

pub fn pinned_resource_ref(
    urn: &str,
    pin: &CatalogPin,
    locators: Vec<vam_spec::v1::ResourceLocator>,
) -> ResourceRef {
    ResourceRef {
        resource_id: Some(ResourceId {
            urn: urn.to_string(),
        }),
        version: Some(pin.version.clone()),
        digest: Some(Digest {
            algorithm: SHA256,
            hex_hash: pin.digest.clone(),
        }),
        locators,
    }
}

/// Load the agent for a run: honor a pinned `agent_ref` version; otherwise latest released.
///
/// A draft pin (`X.Y.Z-draft`, any numbers) loads that owner draft and is not rewritten to
/// latest released. Foreign drafts are NotFound unless `include_foreign_drafts`.
pub fn load_agent_for_run(
    store: &SqliteWorkflowStore,
    spec: &RunSpec,
    urn: &str,
    owner_user_id: &str,
    include_foreign_drafts: bool,
) -> Result<AgentSpec, OrchestratorError> {
    match spec.agent_ref.as_ref().and_then(|r| r.version.as_ref()) {
        Some(version) => {
            store.get_agent_at_version_visible(urn, version, owner_user_id, include_foreign_drafts)
        }
        None => store.get_agent_visible(urn, owner_user_id, include_foreign_drafts),
    }
}

/// [`load_agent_for_run`] for a session that was already admitted (boot, resume, the per-tool-call
/// policy read): same pin handling, but the agent access rule is not applied, so a revoke never
/// breaks a running session (A7). Admission sites use [`load_agent_for_run`].
pub fn load_admitted_agent(
    store: &SqliteWorkflowStore,
    spec: &RunSpec,
    urn: &str,
    owner_user_id: &str,
    include_foreign_drafts: bool,
) -> Result<AgentSpec, OrchestratorError> {
    match spec.agent_ref.as_ref().and_then(|r| r.version.as_ref()) {
        Some(version) => {
            store.get_agent_at_version_admitted(urn, version, owner_user_id, include_foreign_drafts)
        }
        None => store.get_agent_admitted(urn, owner_user_id, include_foreign_drafts),
    }
}

/// If the workspace already pins a catalog version, require CreateRun to match
/// it (or apply it when the run omitted version).
///
/// A run that names no `agent_ref` takes the workspace's bound agent URN and
/// version, so a draft-bound workspace loads the caller's draft (see
/// [`load_agent_for_run`], which keeps the owner check). The digest is left for
/// `record_resolved_refs` to fill from the current pin. Callers pass `None` for
/// `bound_meta` when the run supplies its own inline agent.
pub fn apply_workspace_version_pin(
    spec: &mut RunSpec,
    bound_meta: Option<&vam_storage::WorkspaceMeta>,
    workspace_id: &str,
) -> Result<(), OrchestratorError> {
    let Some(bound) = bound_meta.and_then(|m| m.agent_version.as_ref()) else {
        return Ok(());
    };
    let bound_spec = bound.to_spec();
    match spec.agent_ref.as_mut() {
        Some(existing) => match existing.version.as_ref() {
            Some(requested) => {
                let requested_pin = vam_storage::WorkspaceAgentVersion::from_spec(requested);
                if !bound.matches(&requested_pin) {
                    return Err(OrchestratorError::AgentWorkspaceVersionMismatch {
                        workspace_id: workspace_id.to_string(),
                        bound: bound.label(),
                        requested: requested_pin.label(),
                    });
                }
                Ok(())
            }
            None => {
                existing.version = Some(bound_spec);
                Ok(())
            }
        },
        None => {
            let Some(urn) = bound_meta
                .and_then(|m| m.agent_urn.as_deref())
                .filter(|u| !u.is_empty())
            else {
                return Ok(());
            };
            // The stored digest is not copied: it is written on the first bind only,
            // and a draft changes after that. `pin_agent_ref` fills the digest from
            // the owner-checked pin at the bound version.
            spec.agent_ref = Some(ResourceRef {
                resource_id: Some(ResourceId {
                    urn: urn.to_string(),
                }),
                version: Some(bound_spec),
                digest: None,
                locators: Vec::new(),
            });
            Ok(())
        }
    }
}

/// Resolve unversioned skill/playbook/policy refs, pin `agent_ref`, and write
/// [`RunSpec::resolved_refs`] (version + digest) onto the run spec.
///
/// Every ref goes through [`resolve_ref_for_owner`]. A draft run (the agent pin is
/// `-draft`) resolves floating refs for the agent's owner; any other run resolves
/// them to the latest release. Refs covered: `playbook_ref`, `skill_refs`,
/// `default_playbook_ref`, `allowed_playbook_refs`, `overlay.skills.add`, the
/// playbook's `requires` skills and the policy refs. `extends` and
/// `evaluator_refs` stay out until the runtime merge is wired.
///
/// Draft pins resolve only for the resolving owner (or any owner when
/// `include_foreign_drafts`); released pins are visible to every member.
pub fn record_resolved_refs(
    store: &SqliteWorkflowStore,
    spec: &mut RunSpec,
    agent: &AgentSpec,
    owner_user_id: &str,
    include_foreign_drafts: bool,
) -> Result<(), OrchestratorError> {
    let caller = Viewer {
        owner_user_id,
        include_foreign_drafts,
        mode: ResolveMode::Release,
    };
    pin_agent_ref(store, spec, agent, caller)?;

    // The agent pin is now set, so the mode is the one the session start derives.
    let (owner, mode) = run_resolve_context(store, spec, owner_user_id);
    let viewer = Viewer {
        owner_user_id: &owner,
        include_foreign_drafts,
        mode,
    };

    let mut resolved = Vec::new();
    if let Some(playbook_ref) = spec.playbook_ref.take() {
        // The caller named this ref: a caller who does not author never takes a draft.
        let playbook_viewer = Viewer {
            mode: caller_ref_mode(store, owner_user_id, mode),
            ..viewer
        };
        let pinned = resolve_ref_by_urn(store, &playbook_ref, playbook_viewer)?;
        spec.playbook_ref = Some(pinned.clone());
        if !is_unversioned(&playbook_ref) || pinned.version.is_some() {
            resolved.push(pinned);
        }
    }

    for skill_ref in &agent.skill_refs {
        resolved.push(resolve_ref_by_urn(store, skill_ref, viewer)?);
    }

    collect_agent_playbook_and_overlay_refs(store, agent, &mut resolved, viewer)?;

    collect_playbook_skill_refs(store, spec, &mut resolved, viewer)?;

    collect_policy_refs(agent, &mut resolved, store, viewer)?;

    spec.resolved_refs = resolved;
    Ok(())
}

/// Which owner's drafts a run resolves floating refs against, and in which mode.
///
/// A draft run (`agent_ref` version is `-draft`) uses the agent row's
/// `owner_user_id`, so an operator running someone's draft agent resolves that
/// owner's drafts, not their own. Any other run is a release run for `caller`. An
/// inline agent with no catalog row falls back to `caller`. Session start calls this
/// too, so a run and its boot agree on the pin.
pub fn run_resolve_context(
    store: &SqliteWorkflowStore,
    spec: &RunSpec,
    caller: &str,
) -> (String, ResolveMode) {
    let draft_run = spec
        .agent_ref
        .as_ref()
        .and_then(|r| r.version.as_ref())
        .is_some_and(|v| v.prerelease == "draft");
    if !draft_run {
        return (caller.to_string(), ResolveMode::Release);
    }
    let owner = spec
        .agent_ref
        .as_ref()
        .and_then(|r| r.resource_id.as_ref())
        .map(|id| store.agent_draft_owner(&id.urn));
    let owner = match owner {
        Some(Ok(Some(owner))) => owner,
        // A lookup error fails closed: `local` is a Member, so the run is released-only.
        Some(Err(_)) => crate::users::LOCAL_USER_ID.to_string(),
        Some(Ok(None)) | None => caller.to_string(),
    };
    let mode = draft_mode_for_owner(store, &owner);
    (owner, mode)
}

/// True when `user_id` authors catalog parts by its LIVE role: Operator, OrgAdmin or
/// Publisher, and the bootstrap owner always. A missing users row or a lookup error does
/// not author (fail closed). The legacy owner `local` is a Member.
pub(crate) fn owner_authors(store: &SqliteWorkflowStore, user_id: &str) -> bool {
    if user_id == crate::users::OPERATOR_USER_ID {
        return true;
    }
    store
        .get_user(user_id)
        .map(|user| user.role != crate::users::UserRole::Member)
        .unwrap_or(false)
}

/// The mode a draft agent's run resolves every ref in: [`ResolveMode::Draft`] when the
/// agent row's owner authors, else [`ResolveMode::ReleasedOnly`].
pub(crate) fn draft_mode_for_owner(store: &SqliteWorkflowStore, owner: &str) -> ResolveMode {
    if owner_authors(store, owner) {
        ResolveMode::Draft
    } else {
        ResolveMode::ReleasedOnly
    }
}

/// The mode for a ref the caller names at start (a run's `playbook_ref`, a grant's
/// playbook): `base` when the caller authors, else [`ResolveMode::ReleasedOnly`], so a
/// Member's explicit `-draft` ref is refused. A released agent's frozen refs keep `base`.
pub(crate) fn caller_ref_mode(
    store: &SqliteWorkflowStore,
    caller: &str,
    base: ResolveMode,
) -> ResolveMode {
    if base == ResolveMode::ReleasedOnly || !owner_authors(store, caller) {
        ResolveMode::ReleasedOnly
    } else {
        base
    }
}

/// Session start in [`ResolveMode::ReleasedOnly`]: a recorded draft pin (the run was
/// recorded while its owner authored) is refused, so a demoted owner's old run fails closed.
pub(crate) fn require_no_draft_pins(
    spec: &RunSpec,
    mode: ResolveMode,
) -> Result<(), OrchestratorError> {
    if mode != ResolveMode::ReleasedOnly {
        return Ok(());
    }
    for pinned in spec.resolved_refs.iter().chain(spec.playbook_ref.iter()) {
        let draft = pinned
            .version
            .as_ref()
            .is_some_and(|v| v.prerelease.eq_ignore_ascii_case("draft"));
        let urn = pinned
            .resource_id
            .as_ref()
            .map(|id| id.urn.as_str())
            .unwrap_or("");
        if draft && matches!(artifact_kind(urn), Some("skill" | "playbook" | "policy")) {
            return Err(OrchestratorError::InvalidContract(format!(
                "'{urn}': drafts are not available to this agent"
            )));
        }
    }
    Ok(())
}

/// The one message for every ref that is not an existing release, whatever the cause
/// (missing URN, someone else's draft, the caller's own draft, an explicit `-draft`, a
/// version with no release, a digest mismatch): nothing to learn about other users' rows.
pub(crate) fn not_released(kind: ArtifactKind, urn: &str) -> OrchestratorError {
    OrchestratorError::InvalidContract(format!(
        "{} '{urn}' has no released version; an agent saved by a member may use released \
         skills, playbooks and policies only",
        kind.noun()
    ))
}

/// Whose draft is a floating ref allowed to land on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveMode {
    /// A released agent, or any run that is not a draft run: a floating ref is the
    /// latest release. (A released agent's refs are frozen at release.)
    Release,
    /// A draft run: a floating ref is the owner's own draft of the dependency when
    /// they have one, else the latest release.
    Draft,
    /// An agent owned by someone who may not author (a Member), and the save of such an
    /// agent: only releases count. A floating ref is the latest release (the owner's draft
    /// is never taken); `X.Y.Z` is that release or an error (no "kept as-is" for a version
    /// the catalog does not hold); `X.Y.Z-draft` is an error. `include_foreign_drafts` is
    /// ignored.
    ReleasedOnly,
}

/// Catalog kinds a floating ref can name. Agents are pinned on `RunSpec.agent_ref`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactKind {
    Skill,
    Playbook,
    Policy,
}

impl ArtifactKind {
    /// Kind from a `urn:vam:<kind>:...` URN; agent and unknown kinds are errors.
    pub fn from_urn(urn: &str) -> Result<Self, OrchestratorError> {
        match artifact_kind(urn) {
            Some("skill") => Ok(Self::Skill),
            Some("playbook") => Ok(Self::Playbook),
            Some("policy") => Ok(Self::Policy),
            Some("agent") => Err(OrchestratorError::InvalidContract(
                "agent refs must stay pinned on RunSpec.agent_ref".to_string(),
            )),
            Some(other) => Err(OrchestratorError::InvalidContract(format!(
                "cannot resolve catalog kind '{other}' for '{urn}'"
            ))),
            None => Err(OrchestratorError::InvalidContract(format!(
                "unsupported catalog URN '{urn}'"
            ))),
        }
    }

    pub(crate) fn noun(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Playbook => "playbook",
            Self::Policy => "policy",
        }
    }
}

/// A resolved ref: the pinned [`ResourceRef`] (version and digest, locators kept)
/// and the channel it was taken from.
#[derive(Clone, Debug, PartialEq)]
pub struct PinnedRef {
    pub reference: ResourceRef,
    /// `Draft` when the pin is the owner's working copy, `Released` otherwise.
    pub channel: SkillChannel,
}

/// Who is resolving a ref, and in which mode. Draft rows are visible to their owner only.
#[derive(Clone, Copy)]
struct Viewer<'a> {
    owner_user_id: &'a str,
    include_foreign_drafts: bool,
    mode: ResolveMode,
}

/// The one place a catalog ref becomes a pin. Run recording and session start both call it.
///
/// `owner` is the agent row's `owner_user_id`: the only user whose drafts count.
/// Anyone else's draft is never used.
///
/// | ref | [`ResolveMode::Release`] | [`ResolveMode::Draft`] | [`ResolveMode::ReleasedOnly`] |
/// |---|---|---|---|
/// | no version | latest release | `owner`'s draft if they have one, else latest release | latest release |
/// | `X.Y.Z` | that release | that release | that release, else an error (never kept as-is) |
/// | `X.Y.Z-draft` | `owner`'s draft, else NotFound | `owner`'s draft, else NotFound | an error |
///
/// A floating ref with neither a release nor an owner draft fails with
/// `"<table> '<urn>' has no published version"`. An explicit digest on a versioned ref
/// must match the catalog; an absent, empty or [`EMPTY_SHA256_HEX`] digest means
/// "follow the catalog". A digest on a floating ref is ignored.
///
/// The result carries the version and digest to record and the channel used. Callers
/// freezing refs (release) pass [`ResolveMode::Release`] and refuse a
/// [`SkillChannel::Draft`] channel.
///
/// Errors: `InvalidContract` for a missing URN, a URN not of `kind`, or a digest
/// mismatch; `NotFound` for a missing or foreign explicit draft.
pub fn resolve_ref_for_owner(
    store: &SqliteWorkflowStore,
    kind: ArtifactKind,
    resource_ref: &ResourceRef,
    owner: &str,
    mode: ResolveMode,
) -> Result<PinnedRef, OrchestratorError> {
    resolve_ref_visible(store, kind, resource_ref, owner, mode, false)
}

/// [`resolve_ref_for_owner`] with the operator bypass for explicit `-draft` refs
/// (`include_foreign_drafts`). The floating-ref owner-draft rule never uses it.
pub(crate) fn resolve_ref_visible(
    store: &SqliteWorkflowStore,
    kind: ArtifactKind,
    resource_ref: &ResourceRef,
    owner: &str,
    mode: ResolveMode,
    include_foreign_drafts: bool,
) -> Result<PinnedRef, OrchestratorError> {
    resolve_inner(
        store,
        kind,
        resource_ref,
        Viewer {
            owner_user_id: owner,
            include_foreign_drafts,
            mode,
        },
    )
}

/// Overlay run-recorded skill pins onto the agent copy used at session start.
pub fn overlay_resolved_refs(agent: &mut AgentSpec, spec: &RunSpec) {
    for pinned in &spec.resolved_refs {
        let Some(urn) = pinned.resource_id.as_ref().map(|r| r.urn.as_str()) else {
            continue;
        };
        if artifact_kind(urn) != Some("skill") {
            continue;
        }
        if let Some(existing) = agent
            .skill_refs
            .iter_mut()
            .find(|r| r.resource_id.as_ref().map(|id| id.urn.as_str()) == Some(urn))
        {
            *existing = pinned.clone();
        } else {
            agent.skill_refs.push(pinned.clone());
        }
    }
}

/// Pin skill refs declared on the admitted playbook's `requires`.
///
/// Skills already pinned from the agent are left as they are. A playbook with
/// no `requires`, or a pin whose spec is not in the catalog, adds nothing.
fn collect_playbook_skill_refs(
    store: &SqliteWorkflowStore,
    spec: &RunSpec,
    resolved: &mut Vec<ResourceRef>,
    viewer: Viewer<'_>,
) -> Result<(), OrchestratorError> {
    let Some(playbook_ref) = spec.playbook_ref.as_ref() else {
        return Ok(());
    };
    let Some(urn) = playbook_ref
        .resource_id
        .as_ref()
        .map(|id| id.urn.as_str())
        .filter(|urn| !urn.is_empty())
    else {
        return Ok(());
    };
    let Some(version) = playbook_ref.version.as_ref() else {
        return Ok(());
    };
    let playbook = match store.get_playbook_at_version(
        urn,
        version,
        viewer.owner_user_id,
        viewer.include_foreign_drafts,
    ) {
        Ok(playbook) => playbook,
        // A draft pin that is missing or not the caller's must not be skipped.
        Err(err @ OrchestratorError::NotFound(_)) if version.prerelease == "draft" => {
            return Err(err)
        }
        Err(OrchestratorError::NotFound(_)) => return Ok(()),
        Err(err) => return Err(err),
    };
    for skill_ref in vam_spec::required_skill_refs(&playbook) {
        let skill_urn = skill_ref
            .resource_id
            .as_ref()
            .map(|id| id.urn.as_str())
            .unwrap_or("");
        if skill_urn.is_empty() {
            continue;
        }
        if resolved.iter().any(|existing| {
            existing.resource_id.as_ref().map(|id| id.urn.as_str()) == Some(skill_urn)
        }) {
            continue;
        }
        resolved.push(resolve_ref_by_urn(store, skill_ref, viewer)?);
    }
    Ok(())
}

fn pin_agent_ref(
    store: &SqliteWorkflowStore,
    spec: &mut RunSpec,
    agent: &AgentSpec,
    viewer: Viewer<'_>,
) -> Result<(), OrchestratorError> {
    let urn = crate::run_context::agent_urn(agent);
    if urn.is_empty() {
        return Ok(());
    }

    match spec.agent_ref.as_mut() {
        None => {
            let pin = store.latest_released_catalog_pin("agents", &urn)?;
            spec.agent_ref = Some(pinned_resource_ref(&urn, &pin, Vec::new()));
        }
        Some(existing) if existing.version.is_none() => {
            let pin = store.latest_released_catalog_pin("agents", &urn)?;
            existing.version = Some(pin.version);
            if existing
                .digest
                .as_ref()
                .map(|d| d.hex_hash.as_str())
                .is_none_or(is_unpinned_digest)
            {
                existing.digest = Some(Digest {
                    algorithm: SHA256,
                    hex_hash: pin.digest,
                });
            }
            if existing.resource_id.is_none() {
                existing.resource_id = Some(ResourceId { urn });
            }
        }
        Some(existing) => {
            if existing.resource_id.is_none() {
                existing.resource_id = Some(ResourceId { urn: urn.clone() });
            }
            let version = existing.version.clone().ok_or_else(|| {
                OrchestratorError::InvalidContract("agent_ref version required".to_string())
            })?;
            if digest_is_unpinned(existing) {
                if let Ok(pin) = store.catalog_pin_at_version(
                    "agents",
                    &urn,
                    &version,
                    viewer.owner_user_id,
                    viewer.include_foreign_drafts,
                ) {
                    existing.digest = Some(Digest {
                        algorithm: SHA256,
                        hex_hash: pin.digest,
                    });
                }
            }
        }
    }
    Ok(())
}

fn collect_policy_refs(
    agent: &AgentSpec,
    resolved: &mut Vec<ResourceRef>,
    store: &SqliteWorkflowStore,
    viewer: Viewer<'_>,
) -> Result<(), OrchestratorError> {
    let mut refs = Vec::new();
    if let Some(cap) = &agent.capability_policy {
        if let Some(r) = &cap.policy_ref {
            refs.push(r.clone());
        }
    }
    if let Some(hitl) = &agent.hitl_policy {
        if let Some(r) = &hitl.policy_ref {
            refs.push(r.clone());
        }
    }
    if let Some(tel) = &agent.telemetry_policy {
        if let Some(r) = &tel.policy_ref {
            refs.push(r.clone());
        }
    }
    if let Some(comp) = &agent.completion_policy {
        if let Some(r) = &comp.policy_ref {
            refs.push(r.clone());
        }
    }
    if let Some(permissions) = &agent.permissions {
        refs.extend(permissions.advanced.iter().cloned());
    }
    for policy_ref in refs {
        if policy_ref.resource_id.is_none() {
            continue;
        }
        resolved.push(resolve_ref_by_urn(store, &policy_ref, viewer)?);
    }
    Ok(())
}

/// Resolve a ref whose kind is read from its URN.
fn resolve_ref_by_urn(
    store: &SqliteWorkflowStore,
    resource_ref: &ResourceRef,
    viewer: Viewer<'_>,
) -> Result<ResourceRef, OrchestratorError> {
    let urn = ref_urn(resource_ref)?;
    let kind = ArtifactKind::from_urn(urn)?;
    Ok(resolve_inner(store, kind, resource_ref, viewer)?.reference)
}

fn ref_urn(resource_ref: &ResourceRef) -> Result<&str, OrchestratorError> {
    resource_ref
        .resource_id
        .as_ref()
        .map(|r| r.urn.as_str())
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            OrchestratorError::InvalidContract(
                "ResourceRef.resource_id.urn is required".to_string(),
            )
        })
}

/// Pin the agent's default and allowed playbooks and its overlay skill additions.
///
/// A ref already resolved for this run (same URN) is not recorded twice.
fn collect_agent_playbook_and_overlay_refs(
    store: &SqliteWorkflowStore,
    agent: &AgentSpec,
    resolved: &mut Vec<ResourceRef>,
    viewer: Viewer<'_>,
) -> Result<(), OrchestratorError> {
    let overlay_skills = agent
        .overlay
        .as_ref()
        .and_then(|o| o.skills.as_ref())
        .map(|s| s.add.as_slice())
        .unwrap_or_default();
    let refs = agent
        .default_playbook_ref
        .iter()
        .chain(agent.allowed_playbook_refs.iter())
        .chain(overlay_skills.iter());
    for candidate in refs {
        // An unset ref (no URN) is an absent optional field, not an error.
        if candidate
            .resource_id
            .as_ref()
            .is_none_or(|id| id.urn.is_empty())
        {
            continue;
        }
        let pinned = resolve_ref_by_urn(store, candidate, viewer)?;
        let urn = pinned.resource_id.as_ref().map(|id| id.urn.as_str());
        if !resolved
            .iter()
            .any(|r| r.resource_id.as_ref().map(|id| id.urn.as_str()) == urn)
        {
            resolved.push(pinned);
        }
    }
    Ok(())
}

fn resolve_inner(
    store: &SqliteWorkflowStore,
    kind: ArtifactKind,
    resource_ref: &ResourceRef,
    viewer: Viewer<'_>,
) -> Result<PinnedRef, OrchestratorError> {
    let urn = ref_urn(resource_ref)?;
    if ArtifactKind::from_urn(urn)? != kind {
        return Err(OrchestratorError::InvalidContract(format!(
            "ref '{urn}' is not a {} URN",
            kind.noun()
        )));
    }
    let kind_noun = kind.noun();

    if viewer.mode == ResolveMode::ReleasedOnly
        && resource_ref
            .version
            .as_ref()
            .is_some_and(|v| v.prerelease.eq_ignore_ascii_case("draft"))
    {
        return Err(OrchestratorError::InvalidContract(format!(
            "{kind_noun} '{urn}': drafts are not available to this agent"
        )));
    }

    let unversioned = is_unversioned(resource_ref);
    let pin = match kind {
        ArtifactKind::Skill => resolve_skill_pin(
            store,
            urn,
            resource_ref.version.as_ref(),
            unversioned,
            viewer,
        ),
        ArtifactKind::Playbook => resolve_table_pin(
            store,
            "playbooks",
            urn,
            resource_ref.version.as_ref(),
            unversioned,
            viewer,
        ),
        ArtifactKind::Policy => resolve_table_pin(
            store,
            "policies",
            urn,
            resource_ref.version.as_ref(),
            unversioned,
            viewer,
        ),
    };

    let pin = match pin {
        Ok(pin) => pin,
        Err(OrchestratorError::InvalidContract(msg))
            if viewer.mode == ResolveMode::ReleasedOnly
                && msg.ends_with("has no published version") =>
        {
            return Err(not_released(kind, urn));
        }
        // A draft pin that is missing or foreign is an error, never kept as-is.
        Err(err @ OrchestratorError::NotFound(_))
            if resource_ref
                .version
                .as_ref()
                .is_some_and(|v| v.prerelease == "draft") =>
        {
            return Err(err)
        }
        Err(OrchestratorError::NotFound(_))
            if !unversioned && viewer.mode == ResolveMode::ReleasedOnly =>
        {
            return Err(OrchestratorError::InvalidContract(format!(
                "{kind_noun} '{urn}' not found in catalog"
            )));
        }
        Err(OrchestratorError::NotFound(_)) if !unversioned => {
            let kept = resource_ref.clone();
            if kept.version.is_none() || digest_is_unpinned(&kept) {
                return Err(OrchestratorError::InvalidContract(format!(
                    "{kind_noun} '{urn}' not found in catalog"
                )));
            }
            let channel = channel_of(kept.version.as_ref());
            return Ok(PinnedRef {
                reference: kept,
                channel,
            });
        }
        Err(err) => return Err(err),
    };

    let mut pinned = pinned_resource_ref(urn, &pin, resource_ref.locators.clone());
    if !unversioned {
        if let Some(version) = &resource_ref.version {
            pinned.version = Some(version.clone());
        }
        if let Some(digest) = &resource_ref.digest {
            if !is_unpinned_digest(&digest.hex_hash) {
                if digest.hex_hash != pin.digest {
                    return Err(OrchestratorError::InvalidContract(format!(
                        "ref '{urn}' digest mismatch: pin {}, catalog {}",
                        digest.hex_hash, pin.digest
                    )));
                }
                pinned.digest = Some(digest.clone());
            }
        }
    }
    pinned
        .validate()
        .map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?;
    let channel = channel_of(pinned.version.as_ref());
    if viewer.mode == ResolveMode::ReleasedOnly && channel == SkillChannel::Draft {
        return Err(OrchestratorError::InvalidContract(format!(
            "{kind_noun} '{urn}': drafts are not available to this agent"
        )));
    }
    Ok(PinnedRef {
        reference: pinned,
        channel,
    })
}

fn channel_of(version: Option<&Version>) -> SkillChannel {
    if version.is_some_and(|v| v.prerelease == "draft") {
        SkillChannel::Draft
    } else {
        SkillChannel::Released
    }
}

/// The version a draft lookup uses. Drafts are found by URN, so the numbers are a slot only.
fn draft_slot_version() -> Version {
    Version {
        major: 1,
        minor: 0,
        patch: 0,
        prerelease: "draft".to_string(),
        build: String::new(),
    }
}

fn resolve_skill_pin(
    store: &SqliteWorkflowStore,
    urn: &str,
    version: Option<&Version>,
    unversioned: bool,
    viewer: Viewer<'_>,
) -> Result<CatalogPin, OrchestratorError> {
    if unversioned && viewer.mode == ResolveMode::Draft {
        if let Some(pin) = owner_draft_skill_pin(store, urn, viewer.owner_user_id)? {
            return Ok(pin);
        }
    }
    let channel = match version {
        Some(v) if v.prerelease == "draft" => SkillChannel::Draft,
        _ => SkillChannel::Released,
    };
    match store.get_skill_package_for_query_visible(
        urn,
        version,
        channel,
        viewer.owner_user_id,
        viewer.include_foreign_drafts,
    ) {
        Ok(package) => {
            let pkg_version = package.version.clone().ok_or_else(|| {
                OrchestratorError::CorruptState(format!(
                    "skill package '{urn}' missing version after lookup"
                ))
            })?;
            Ok(CatalogPin {
                version: pkg_version,
                digest: package.digest,
            })
        }
        Err(OrchestratorError::NotFound(_)) => {
            resolve_table_pin(store, "skills", urn, version, unversioned, viewer)
        }
        Err(err) => Err(err),
    }
}

/// The owner's draft skill, if they have one. A draft that is absent or someone
/// else's is `None`; operators never get another user's draft here.
fn owner_draft_skill_pin(
    store: &SqliteWorkflowStore,
    urn: &str,
    owner: &str,
) -> Result<Option<CatalogPin>, OrchestratorError> {
    match store.get_skill_package_for_query_visible(urn, None, SkillChannel::Draft, owner, false) {
        Ok(package) => {
            let mut version = package.version.clone().ok_or_else(|| {
                OrchestratorError::CorruptState(format!(
                    "skill package '{urn}' missing version after lookup"
                ))
            })?;
            version.prerelease = "draft".to_string();
            Ok(Some(CatalogPin {
                version,
                digest: package.digest,
            }))
        }
        Err(OrchestratorError::NotFound(_)) => owner_draft_table_pin(store, "skills", urn, owner),
        Err(err) => Err(err),
    }
}

fn owner_draft_table_pin(
    store: &SqliteWorkflowStore,
    table: &str,
    urn: &str,
    owner: &str,
) -> Result<Option<CatalogPin>, OrchestratorError> {
    match store.catalog_pin_at_version(table, urn, &draft_slot_version(), owner, false) {
        Ok(pin) => Ok(Some(pin)),
        Err(OrchestratorError::NotFound(_)) => Ok(None),
        Err(err) => Err(err),
    }
}

fn resolve_table_pin(
    store: &SqliteWorkflowStore,
    table: &str,
    urn: &str,
    version: Option<&Version>,
    unversioned: bool,
    viewer: Viewer<'_>,
) -> Result<CatalogPin, OrchestratorError> {
    if unversioned && viewer.mode == ResolveMode::Draft && table != "skills" {
        if let Some(pin) = owner_draft_table_pin(store, table, urn, viewer.owner_user_id)? {
            return Ok(pin);
        }
    }
    let result = match version {
        Some(v) => store.catalog_pin_at_version(
            table,
            urn,
            v,
            viewer.owner_user_id,
            viewer.include_foreign_drafts,
        ),
        None => store.latest_released_catalog_pin(table, urn),
    };
    match result {
        Err(OrchestratorError::NotFound(_)) if unversioned => Err(
            OrchestratorError::InvalidContract(format!("{table} '{urn}' has no published version")),
        ),
        other => other,
    }
}

fn artifact_kind(urn: &str) -> Option<&str> {
    urn.strip_prefix("urn:vam:")?.split(':').next()
}

/// True for the two resolver failures that mean "nothing released to pin": a floating
/// ref with no release, and a versioned ref the catalog does not hold. Release uses
/// this to list such dependencies instead of failing on the first.
pub(crate) fn is_missing_release_error(err: &OrchestratorError) -> bool {
    match err {
        OrchestratorError::NotFound(_) => true,
        OrchestratorError::InvalidContract(msg) => {
            msg.ends_with("has no published version") || msg.ends_with("not found in catalog")
        }
        _ => false,
    }
}

/// True for an absent, empty or deprecated [`EMPTY_SHA256_HEX`] digest: "follow the catalog".
pub(crate) fn is_unpinned_digest(hex_hash: &str) -> bool {
    hex_hash.is_empty() || hex_hash == EMPTY_SHA256_HEX
}

fn digest_is_unpinned(resource_ref: &ResourceRef) -> bool {
    resource_ref
        .digest
        .as_ref()
        .is_none_or(|d| is_unpinned_digest(&d.hex_hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::LOCAL_USER_ID;
    use vam_spec::v1::{
        agent_spec::InstructionSource, PlaybookRequirements, PlaybookSpec, PlaybookStep,
        SkillChannel, SkillManifest, SkillPackageRecord, StepMode,
    };

    const SKILL_URN: &str = "urn:vam:skill:vam.local/vam/assistant-tools";
    const AGENT_URN: &str = "urn:vam:agent:local.vam.dev/acme/resolver";
    const PLAYBOOK_URN: &str = "urn:vam:playbook:local.vam.dev/acme/resolver-pb";
    const DIGEST_100: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_110: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const DIGEST_200: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const DIGEST_DRAFT: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn store() -> SqliteWorkflowStore {
        SqliteWorkflowStore::in_memory().expect("store")
    }

    fn test_agent(skill_refs: Vec<ResourceRef>) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId {
                urn: AGENT_URN.to_string(),
            }),
            display_name: "Resolver".to_string(),
            description: "test".to_string(),
            role: "tester".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions("go".into())),
            allow_any_playbook: true,
            skill_refs,
            ..Default::default()
        }
    }

    fn test_playbook() -> PlaybookSpec {
        PlaybookSpec {
            activation: Some(vam_spec::v1::PlaybookActivation::asking(
                "Use when the request matches this test playbook.",
            )),
            resource_id: Some(ResourceId {
                urn: PLAYBOOK_URN.to_string(),
            }),
            display_name: "pb".to_string(),
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

    fn package(
        digest: &str,
        major: i32,
        minor: i32,
        patch: i32,
        prerelease: &str,
        channel: SkillChannel,
    ) -> SkillPackageRecord {
        SkillPackageRecord {
            resource_id: Some(ResourceId {
                urn: SKILL_URN.to_string(),
            }),
            version: Some(Version {
                major,
                minor,
                patch,
                prerelease: prerelease.to_string(),
                build: String::new(),
            }),
            channel: channel as i32,
            digest: digest.to_string(),
            blob_relpath: format!("local/sha256-{digest}.tar.gz"),
            size_bytes: 8,
            spec: Some(SkillManifest {
                resource_id: Some(ResourceId {
                    urn: SKILL_URN.to_string(),
                }),
                version: Some(Version {
                    major,
                    minor,
                    patch,
                    prerelease: prerelease.to_string(),
                    build: String::new(),
                }),
                description: "Test".to_string(),
                ..Default::default()
            }),
            metadata: None,
            ..Default::default()
        }
    }

    fn unversioned_skill() -> ResourceRef {
        ResourceRef {
            resource_id: Some(ResourceId {
                urn: SKILL_URN.to_string(),
            }),
            version: None,
            digest: None,
            locators: vec![],
        }
    }

    fn run_spec() -> RunSpec {
        RunSpec {
            run_id: Some(ResourceId {
                urn: "urn:vam:run:local.vam.dev/acme/r1".to_string(),
            }),
            playbook_ref: Some(ResourceRef {
                resource_id: Some(ResourceId {
                    urn: PLAYBOOK_URN.to_string(),
                }),
                version: None,
                digest: None,
                locators: vec![],
            }),
            workspace_ref_uri: "volume://assistant".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn unversioned_skill_resolves_to_latest_released_not_newer_draft() {
        let store = store();
        store
            .put_skill_package(&package(DIGEST_100, 1, 0, 0, "", SkillChannel::Released))
            .expect("1.0.0");
        store
            .put_skill_package(&package(DIGEST_110, 1, 1, 0, "", SkillChannel::Released))
            .expect("1.1.0");
        store
            .put_skill_package(&package(DIGEST_200, 2, 0, 0, "draft", SkillChannel::Draft))
            .expect("2.0.0-draft");
        store
            .put_agent_released_with_owner(&test_agent(vec![unversioned_skill()]), LOCAL_USER_ID)
            .expect("agent");
        store
            .put_playbook_with_owner(&test_playbook(), LOCAL_USER_ID)
            .expect("playbook");

        let agent = store.get_agent(AGENT_URN).expect("agent");
        let mut spec = run_spec();
        record_resolved_refs(&store, &mut spec, &agent, LOCAL_USER_ID, false).expect("resolve");

        let skill = spec
            .resolved_refs
            .iter()
            .find(|r| r.resource_id.as_ref().map(|id| id.urn.as_str()) == Some(SKILL_URN))
            .expect("skill pin");
        let version = skill.version.as_ref().expect("version");
        assert_eq!((version.major, version.minor, version.patch), (1, 1, 0));
        assert_eq!(skill.digest.as_ref().expect("digest").hex_hash, DIGEST_110);

        let playbook = spec.playbook_ref.as_ref().expect("playbook");
        assert!(playbook.version.is_some());
        assert!(playbook.digest.is_some());
    }

    fn unversioned_ref(urn: &str) -> ResourceRef {
        ResourceRef {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            version: None,
            digest: None,
            locators: vec![],
        }
    }

    fn released_package(urn: &str, digest: &str) -> SkillPackageRecord {
        let mut record = package(digest, 1, 0, 0, "", SkillChannel::Released);
        if let Some(id) = record.resource_id.as_mut() {
            id.urn = urn.to_string();
        }
        if let Some(spec) = record.spec.as_mut() {
            if let Some(id) = spec.resource_id.as_mut() {
                id.urn = urn.to_string();
            }
        }
        record
    }

    #[test]
    fn playbook_declaring_two_skills_resolves_both() {
        const JIRA: &str = "urn:vam:skill:vam.local/vam/jira-tools";
        const SLACK: &str = "urn:vam:skill:vam.local/vam/slack-tools";
        let store = store();
        store
            .put_skill_package(&released_package(JIRA, DIGEST_100))
            .expect("jira");
        store
            .put_skill_package(&released_package(SLACK, DIGEST_110))
            .expect("slack");
        store
            .put_agent_released_with_owner(&test_agent(vec![]), LOCAL_USER_ID)
            .expect("agent");

        let mut playbook = test_playbook();
        playbook.requires = Some(PlaybookRequirements {
            skills: vec![unversioned_ref(JIRA), unversioned_ref(SLACK)],
            capabilities: vec!["tool:jira.search".into(), "tool:slack.post".into()],
        });
        store
            .put_playbook_with_owner(&playbook, LOCAL_USER_ID)
            .expect("playbook");

        let agent = store.get_agent(AGENT_URN).expect("agent");
        let mut spec = run_spec();
        record_resolved_refs(&store, &mut spec, &agent, LOCAL_USER_ID, false).expect("resolve");

        for (urn, digest) in [(JIRA, DIGEST_100), (SLACK, DIGEST_110)] {
            let skill = spec
                .resolved_refs
                .iter()
                .find(|r| r.resource_id.as_ref().map(|id| id.urn.as_str()) == Some(urn))
                .unwrap_or_else(|| panic!("missing pin for {urn}"));
            let version = skill.version.as_ref().expect("version");
            assert_eq!((version.major, version.minor, version.patch), (1, 0, 0));
            assert_eq!(skill.digest.as_ref().expect("digest").hex_hash, digest);
        }

        let mut mounted = test_agent(vec![]);
        overlay_resolved_refs(&mut mounted, &spec);
        let mounted_urns: Vec<String> = mounted
            .skill_refs
            .iter()
            .filter_map(|skill_ref| skill_ref.resource_id.as_ref().map(|id| id.urn.clone()))
            .collect();
        assert!(mounted_urns.iter().any(|urn| urn == JIRA));
        assert!(mounted_urns.iter().any(|urn| urn == SLACK));
    }

    #[test]
    fn missing_published_skill_fails_closed() {
        let store = store();
        store
            .put_skill_package(&package(
                DIGEST_DRAFT,
                1,
                0,
                0,
                "draft",
                SkillChannel::Draft,
            ))
            .expect("draft only");
        store
            .put_agent_released_with_owner(&test_agent(vec![unversioned_skill()]), LOCAL_USER_ID)
            .expect("agent");
        store
            .put_playbook_with_owner(&test_playbook(), LOCAL_USER_ID)
            .expect("playbook");

        let agent = store.get_agent(AGENT_URN).expect("agent");
        let mut spec = run_spec();
        let err = record_resolved_refs(&store, &mut spec, &agent, LOCAL_USER_ID, false)
            .expect_err("closed");
        assert!(matches!(err, OrchestratorError::InvalidContract(_)));
        assert!(
            err.to_string().contains("no published version")
                || err.to_string().contains("not found")
        );
    }

    #[test]
    fn run_record_stores_resolved_version_and_digest() {
        let store = store();
        store
            .put_skill_package(&package(DIGEST_100, 1, 0, 0, "", SkillChannel::Released))
            .expect("released");
        store
            .put_agent_released_with_owner(&test_agent(vec![unversioned_skill()]), LOCAL_USER_ID)
            .expect("agent");
        store
            .put_playbook_with_owner(&test_playbook(), LOCAL_USER_ID)
            .expect("playbook");

        let agent = store.get_agent(AGENT_URN).expect("agent");
        let mut spec = run_spec();
        record_resolved_refs(&store, &mut spec, &agent, LOCAL_USER_ID, false).expect("resolve");
        let spec_json = vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &spec).expect("json");
        store
            .persist_run_spec_with_owner(
                "urn:vam:run:local.vam.dev/acme/r1",
                &spec_json,
                Some("sess-1"),
                AGENT_URN,
                Some("idem-1"),
                "local",
                LOCAL_USER_ID,
            )
            .expect("persist");

        let (loaded, _, _) = store
            .get_run_spec_context("urn:vam:run:local.vam.dev/acme/r1")
            .expect("load");
        let loaded: RunSpec =
            vam_spec::encoding::json_to_proto("vam.v1.RunSpec", &loaded.expect("json"))
                .expect("decode");
        let skill = loaded
            .resolved_refs
            .iter()
            .find(|r| r.resource_id.as_ref().map(|id| id.urn.as_str()) == Some(SKILL_URN))
            .expect("recorded");
        assert_eq!(skill.version.as_ref().unwrap().major, 1);
        assert_eq!(skill.digest.as_ref().unwrap().hex_hash, DIGEST_100);
        let agent_ref = loaded.agent_ref.as_ref().expect("agent pin");
        assert!(agent_ref.version.is_some());
        assert!(agent_ref.digest.is_some());
    }

    #[test]
    fn pinned_agent_ref_is_not_rewritten_to_latest() {
        let store = store();
        store
            .put_agent_released_with_owner(&test_agent(vec![]), LOCAL_USER_ID)
            .expect("v1");
        store
            .put_playbook_with_owner(&test_playbook(), LOCAL_USER_ID)
            .expect("playbook");
        let mut v101 = test_agent(vec![]);
        v101.display_name = "1.0.1".to_string();
        let mut bytes = Vec::new();
        prost::Message::encode(&v101, &mut bytes).expect("encode");
        {
            let conn = store.connection.lock().expect("lock");
            let now = crate::catalog::unix_timestamp();
            conn.execute(
                "INSERT INTO agents (
                    urn, major, minor, patch, prerelease, channel, revision, digest,
                    spec_bytes, created_at, updated_at, owner_user_id
                 ) VALUES (?1, 1, 0, 1, '', 'released', 1, 'dig-101', ?2, ?3, ?3, 'local')",
                rusqlite::params![AGENT_URN, bytes, now],
            )
            .expect("1.0.1");
        }

        let mut spec = run_spec();
        spec.agent_ref = Some(ResourceRef {
            resource_id: Some(ResourceId {
                urn: AGENT_URN.to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            digest: Some(Digest {
                algorithm: SHA256,
                hex_hash: "keep-me".to_string(),
            }),
            locators: vec![],
        });
        let loaded = load_agent_for_run(&store, &spec, AGENT_URN, LOCAL_USER_ID, false)
            .expect("pinned agent");
        assert_eq!(loaded.display_name, "Resolver");
        record_resolved_refs(&store, &mut spec, &loaded, LOCAL_USER_ID, false).expect("record");
        let pin = spec.agent_ref.as_ref().unwrap();
        assert_eq!(pin.version.as_ref().unwrap().patch, 0);
        assert_eq!(pin.digest.as_ref().unwrap().hex_hash, "keep-me");
    }

    fn draft_version() -> Version {
        Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: "draft".to_string(),
            build: String::new(),
        }
    }

    fn released_version() -> Version {
        Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: String::new(),
            build: String::new(),
        }
    }

    fn spec_with_version(version: Version) -> RunSpec {
        let mut spec = run_spec();
        spec.playbook_ref = None;
        spec.agent_ref = Some(ResourceRef {
            resource_id: Some(ResourceId {
                urn: AGENT_URN.to_string(),
            }),
            version: Some(version),
            digest: None,
            locators: vec![],
        });
        spec
    }

    #[test]
    fn draft_pin_loads_owner_draft_not_latest_released() {
        let store = store();
        let mut released = test_agent(vec![]);
        released.display_name = "released".to_string();
        store
            .put_agent_released_with_owner(&released, LOCAL_USER_ID)
            .expect("released");
        let mut draft = test_agent(vec![]);
        draft.display_name = "draft".to_string();
        store
            .publish_agent_draft_with_owner(&draft, LOCAL_USER_ID)
            .expect("draft");

        let mut spec = spec_with_version(draft_version());
        let loaded =
            load_agent_for_run(&store, &spec, AGENT_URN, LOCAL_USER_ID, false).expect("draft pin");
        assert_eq!(loaded.display_name, "draft");
        record_resolved_refs(&store, &mut spec, &loaded, LOCAL_USER_ID, false).expect("record");
        let pin = spec.agent_ref.as_ref().unwrap().version.as_ref().unwrap();
        assert_eq!(pin.prerelease, "draft");
        assert_eq!(pin.patch, 0);
    }

    #[test]
    fn missing_released_pin_is_not_found() {
        let store = store();
        store
            .publish_agent_draft_with_owner(&test_agent(vec![]), LOCAL_USER_ID)
            .expect("draft only");
        let spec = spec_with_version(released_version());
        let err = load_agent_for_run(&store, &spec, AGENT_URN, LOCAL_USER_ID, false)
            .expect_err("unpublished 1.0.0");
        assert!(matches!(err, OrchestratorError::NotFound(_)));
        assert!(err.to_string().contains("1.0.0 not found"), "{}", err);
    }

    #[test]
    fn member_cannot_load_foreign_draft_pin() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        store
            .publish_agent_draft_with_owner(&test_agent(vec![]), "alice")
            .expect("alice draft");
        let spec = spec_with_version(draft_version());
        let err =
            load_agent_for_run(&store, &spec, AGENT_URN, "bob", false).expect_err("foreign draft");
        assert!(matches!(err, OrchestratorError::NotFound(_)));
        assert!(
            err.to_string()
                .contains(&format!("no draft of agent '{AGENT_URN}' for this user")),
            "{}",
            err
        );
    }

    #[test]
    fn draft_pin_ignores_version_numbers() {
        let store = store();
        let mut draft = test_agent(vec![]);
        draft.display_name = "draft".to_string();
        store
            .publish_agent_draft_with_owner(&draft, LOCAL_USER_ID)
            .expect("draft");

        for (major, minor, patch) in [(0, 1, 1), (1, 0, 0), (9, 9, 9)] {
            let version = Version {
                major,
                minor,
                patch,
                prerelease: "draft".to_string(),
                build: String::new(),
            };
            let spec = spec_with_version(version);
            let loaded = load_agent_for_run(&store, &spec, AGENT_URN, LOCAL_USER_ID, false)
                .unwrap_or_else(|e| panic!("{major}.{minor}.{patch}-draft: {e}"));
            assert_eq!(loaded.display_name, "draft");
        }
    }

    fn workspace_bound_to(version: &Version, digest: &str) -> vam_storage::WorkspaceMeta {
        vam_storage::WorkspaceMeta {
            workspace_id: "ws-1".to_string(),
            spec_version: "1".to_string(),
            created_at: 0,
            agent_urn: Some(AGENT_URN.to_string()),
            agent_digest: Some(digest.to_string()),
            agent_version: Some(vam_storage::WorkspaceAgentVersion::from_spec(version)),
        }
    }

    #[test]
    fn workspace_draft_pin_without_agent_ref_loads_owner_draft() {
        let store = store();
        let mut released = test_agent(vec![]);
        released.display_name = "released".to_string();
        store
            .put_agent_released_with_owner(&released, LOCAL_USER_ID)
            .expect("released");
        let mut draft = test_agent(vec![]);
        draft.display_name = "draft".to_string();
        store
            .publish_agent_draft_with_owner(&draft, LOCAL_USER_ID)
            .expect("draft");

        let current = store
            .catalog_pin_at_version("agents", AGENT_URN, &draft_version(), LOCAL_USER_ID, false)
            .expect("current draft pin");
        let mut spec = run_spec();
        spec.playbook_ref = None;
        assert!(spec.agent_ref.is_none());
        let meta = workspace_bound_to(&draft_version(), DIGEST_DRAFT);
        apply_workspace_version_pin(&mut spec, Some(&meta), "ws-1").expect("pin");

        let pin = spec
            .agent_ref
            .as_ref()
            .expect("agent_ref set from workspace");
        assert_eq!(pin.resource_id.as_ref().unwrap().urn, AGENT_URN);
        assert!(pin.digest.is_none(), "stored digest must not be copied");
        let loaded =
            load_agent_for_run(&store, &spec, AGENT_URN, LOCAL_USER_ID, false).expect("owner");
        assert_eq!(loaded.display_name, "draft");
        record_resolved_refs(&store, &mut spec, &loaded, LOCAL_USER_ID, false).expect("record");
        assert_eq!(
            spec.agent_ref
                .as_ref()
                .unwrap()
                .digest
                .as_ref()
                .unwrap()
                .hex_hash,
            current.digest
        );
    }

    #[test]
    fn workspace_draft_pin_records_digest_of_edited_draft() {
        let store = store();
        let mut draft = test_agent(vec![]);
        draft.display_name = "draft".to_string();
        store
            .publish_agent_draft_with_owner(&draft, LOCAL_USER_ID)
            .expect("draft");
        let bound = store
            .catalog_pin_at_version("agents", AGENT_URN, &draft_version(), LOCAL_USER_ID, false)
            .expect("bound draft pin");

        // The owner edits the draft after the workspace was bound to it.
        draft.display_name = "edited".to_string();
        store
            .publish_agent_draft_with_owner(&draft, LOCAL_USER_ID)
            .expect("edited draft");
        let edited = store
            .catalog_pin_at_version("agents", AGENT_URN, &draft_version(), LOCAL_USER_ID, false)
            .expect("edited draft pin");
        assert_ne!(edited.digest, bound.digest, "edit must change the digest");

        let mut spec = run_spec();
        spec.playbook_ref = None;
        let meta = workspace_bound_to(&draft_version(), &bound.digest);
        apply_workspace_version_pin(&mut spec, Some(&meta), "ws-1").expect("pin");
        let loaded =
            load_agent_for_run(&store, &spec, AGENT_URN, LOCAL_USER_ID, false).expect("draft");
        assert_eq!(loaded.display_name, "edited");
        record_resolved_refs(&store, &mut spec, &loaded, LOCAL_USER_ID, false).expect("record");
        let recorded = spec.agent_ref.as_ref().unwrap();
        assert_eq!(recorded.digest.as_ref().unwrap().hex_hash, edited.digest);
        assert_ne!(recorded.digest.as_ref().unwrap().hex_hash, bound.digest);
    }

    #[test]
    fn workspace_draft_pin_without_agent_ref_rejects_other_user() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        store
            .publish_agent_draft_with_owner(&test_agent(vec![]), "alice")
            .expect("alice draft");
        store
            .put_agent_released_with_owner(&test_agent(vec![]), "alice")
            .expect("released");

        let mut spec = run_spec();
        let meta = workspace_bound_to(&draft_version(), DIGEST_DRAFT);
        apply_workspace_version_pin(&mut spec, Some(&meta), "ws-1").expect("pin");
        let err = load_agent_for_run(&store, &spec, AGENT_URN, "bob", false)
            .expect_err("bob must not get alice's draft or the release");
        assert!(matches!(err, OrchestratorError::NotFound(_)));
        assert!(
            err.to_string()
                .contains(&format!("no draft of agent '{AGENT_URN}' for this user")),
            "{}",
            err
        );
    }

    #[test]
    fn workspace_released_pin_without_agent_ref_loads_that_release() {
        let store = store();
        let mut released = test_agent(vec![]);
        released.display_name = "released".to_string();
        store
            .put_agent_released_with_owner(&released, LOCAL_USER_ID)
            .expect("released");
        let mut draft = test_agent(vec![]);
        draft.display_name = "draft".to_string();
        store
            .publish_agent_draft_with_owner(&draft, LOCAL_USER_ID)
            .expect("draft");

        let mut spec = run_spec();
        spec.playbook_ref = None;
        let meta = workspace_bound_to(&released_version(), DIGEST_100);
        apply_workspace_version_pin(&mut spec, Some(&meta), "ws-1").expect("pin");

        let pin = spec.agent_ref.as_ref().unwrap();
        assert_eq!(pin.version.as_ref().unwrap().prerelease, "");
        assert!(pin.digest.is_none(), "stored digest must not be copied");
        let loaded =
            load_agent_for_run(&store, &spec, AGENT_URN, LOCAL_USER_ID, false).expect("release");
        assert_eq!(loaded.display_name, "released");
        record_resolved_refs(&store, &mut spec, &loaded, LOCAL_USER_ID, false).expect("record");
        let release = store
            .catalog_pin_at_version(
                "agents",
                AGENT_URN,
                &released_version(),
                LOCAL_USER_ID,
                false,
            )
            .expect("release pin");
        assert_eq!(
            spec.agent_ref
                .as_ref()
                .unwrap()
                .digest
                .as_ref()
                .unwrap()
                .hex_hash,
            release.digest
        );
    }

    #[test]
    fn explicit_agent_ref_mismatch_is_still_rejected() {
        let mut spec = spec_with_version(released_version());
        let meta = workspace_bound_to(&draft_version(), DIGEST_DRAFT);
        let err = apply_workspace_version_pin(&mut spec, Some(&meta), "ws-1")
            .expect_err("explicit 1.0.0 against a draft-bound workspace");
        assert!(
            matches!(err, OrchestratorError::AgentWorkspaceVersionMismatch { .. }),
            "{err}"
        );
    }

    const POLICY_URN: &str = "urn:vam:policy:local.vam.dev/acme/resolver-policy";

    fn draft_ref(urn: &str, digest: &str) -> ResourceRef {
        ResourceRef {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: "draft".to_string(),
                build: String::new(),
            }),
            digest: Some(Digest {
                algorithm: SHA256,
                hex_hash: digest.to_string(),
            }),
            locators: vec![],
        }
    }

    #[test]
    fn foreign_draft_playbook_pin_is_rejected_for_bob_not_alice_or_operator() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        // Callers who author; a Member's explicit `-draft` ref is refused.
        for user in ["alice", "bob"] {
            store
                .set_user_role(user, crate::users::UserRole::Publisher)
                .expect("promote");
        }
        store
            .publish_playbook_draft_with_owner(&test_playbook(), "alice")
            .expect("alice draft");
        let digest = store
            .catalog_pin_at_version(
                "playbooks",
                PLAYBOOK_URN,
                &draft_ref(PLAYBOOK_URN, "").version.expect("v"),
                "alice",
                false,
            )
            .expect("pin")
            .digest;

        let resolve = |user: &str, operator: bool| {
            let mut spec = run_spec();
            spec.playbook_ref = Some(draft_ref(PLAYBOOK_URN, &digest));
            record_resolved_refs(&store, &mut spec, &AgentSpec::default(), user, operator)
        };
        resolve("alice", false).expect("owner");
        resolve("bob", true).expect("operator");
        let err = resolve("bob", false).expect_err("bob");
        assert_eq!(
            err.to_string(),
            format!("not found: no draft of playbook '{PLAYBOOK_URN}' for this user")
        );
    }

    #[test]
    fn foreign_draft_policy_pin_on_agent_is_rejected_for_bob() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        store
            .publish_policy_draft_with_owner(
                POLICY_URN,
                "permit(principal, action, resource);",
                "alice",
            )
            .expect("alice draft");
        let version = draft_ref(POLICY_URN, "").version.expect("v");
        let digest = store
            .catalog_pin_at_version("policies", POLICY_URN, &version, "alice", false)
            .expect("pin")
            .digest;
        let mut agent = test_agent(vec![]);
        agent.capability_policy = Some(vam_spec::v1::CapabilityPolicyRef {
            policy_ref: Some(draft_ref(POLICY_URN, &digest)),
            ..Default::default()
        });

        store
            .put_agent_released_with_owner(&agent, LOCAL_USER_ID)
            .expect("agent");
        let resolve = |user: &str, operator: bool| {
            let mut spec = RunSpec::default();
            record_resolved_refs(&store, &mut spec, &agent, user, operator)
        };
        resolve("alice", false).expect("owner");
        resolve("bob", true).expect("operator");
        let err = resolve("bob", false).expect_err("bob");
        assert_eq!(
            err.to_string(),
            format!("not found: no draft of policy '{POLICY_URN}' for this user")
        );
    }

    const ALICE_AGENT_URN: &str = "urn:vam:agent:local.vam.dev/alice/dev-agent";
    const BOB_AGENT_URN: &str = "urn:vam:agent:local.vam.dev/bob/dev-agent";

    fn floating_agent(urn: &str, skill_refs: Vec<ResourceRef>) -> AgentSpec {
        let mut agent = test_agent(skill_refs);
        agent.resource_id = Some(ResourceId {
            urn: urn.to_string(),
        });
        agent
    }

    /// A run of `agent_urn`'s draft: the agent pin is `-draft`.
    fn draft_run_spec(agent_urn: &str) -> RunSpec {
        let mut spec = run_spec();
        spec.playbook_ref = None;
        spec.agent_ref = Some(ResourceRef {
            resource_id: Some(ResourceId {
                urn: agent_urn.to_string(),
            }),
            version: Some(draft_version()),
            digest: None,
            locators: vec![],
        });
        spec
    }

    fn pinned_skill(spec: &RunSpec) -> &ResourceRef {
        spec.resolved_refs
            .iter()
            .find(|r| r.resource_id.as_ref().map(|id| id.urn.as_str()) == Some(SKILL_URN))
            .expect("skill pin")
    }

    /// alice owns the skill: a release and a draft of it.
    fn store_with_alice_skill() -> SqliteWorkflowStore {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        // A draft owner who authors; a Member's draft agent runs released-only.
        store
            .set_user_role("alice", crate::users::UserRole::Publisher)
            .expect("promote alice");
        store
            .put_skill_package_with_owner(
                &package(DIGEST_100, 1, 0, 0, "", SkillChannel::Released),
                "alice",
            )
            .expect("release");
        store
            .put_skill_package_with_owner(
                &package(DIGEST_DRAFT, 1, 0, 0, "draft", SkillChannel::Draft),
                "alice",
            )
            .expect("alice draft");
        store
    }

    #[test]
    fn floating_skill_on_owners_draft_agent_uses_owners_draft() {
        let store = store_with_alice_skill();
        let agent = floating_agent(ALICE_AGENT_URN, vec![unversioned_skill()]);
        store
            .publish_agent_draft_with_owner(&agent, "alice")
            .expect("alice agent");

        let mut spec = draft_run_spec(ALICE_AGENT_URN);
        record_resolved_refs(&store, &mut spec, &agent, "alice", false).expect("resolve");

        let skill = pinned_skill(&spec);
        assert_eq!(skill.version.as_ref().unwrap().prerelease, "draft");
        assert_eq!(skill.digest.as_ref().unwrap().hex_hash, DIGEST_DRAFT);
    }

    #[test]
    fn floating_skill_on_draft_agent_without_owner_draft_uses_latest_release() {
        let store = store_with_alice_skill();
        // bob's draft agent; the skill and its draft belong to alice.
        let agent = floating_agent(BOB_AGENT_URN, vec![unversioned_skill()]);
        store
            .publish_agent_draft_with_owner(&agent, "bob")
            .expect("bob agent");

        let mut spec = draft_run_spec(BOB_AGENT_URN);
        record_resolved_refs(&store, &mut spec, &agent, "bob", false).expect("resolve");

        let skill = pinned_skill(&spec);
        assert_eq!(skill.version.as_ref().unwrap().prerelease, "");
        assert_eq!(skill.digest.as_ref().unwrap().hex_hash, DIGEST_100);
    }

    #[test]
    fn operator_running_a_draft_agent_resolves_the_agent_owners_draft() {
        let store = store_with_alice_skill();
        let agent = floating_agent(ALICE_AGENT_URN, vec![unversioned_skill()]);
        store
            .publish_agent_draft_with_owner(&agent, "alice")
            .expect("alice agent");

        let mut spec = draft_run_spec(ALICE_AGENT_URN);
        record_resolved_refs(&store, &mut spec, &agent, "bob", true).expect("operator run");
        assert_eq!(
            pinned_skill(&spec).digest.as_ref().unwrap().hex_hash,
            DIGEST_DRAFT,
            "the owner's draft, not the operator's (who has none)"
        );
    }

    #[test]
    fn floating_ref_with_no_release_and_no_owner_draft_fails_with_existing_message() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store
            .put_skill_package_with_owner(
                &package(DIGEST_DRAFT, 1, 0, 0, "draft", SkillChannel::Draft),
                "alice",
            )
            .expect("draft only");
        // Release mode never looks at drafts, even the owner's own.
        let err = resolve_ref_for_owner(
            &store,
            ArtifactKind::Skill,
            &unversioned_skill(),
            "alice",
            ResolveMode::Release,
        )
        .expect_err("no release");
        assert!(err.to_string().contains("no published version"), "{err}");
        store.create_user("bob", "bob").expect("bob");
        let err = resolve_ref_for_owner(
            &store,
            ArtifactKind::Skill,
            &unversioned_skill(),
            "bob",
            ResolveMode::Draft,
        )
        .expect_err("bob has no draft and there is no release");
        assert_eq!(
            err.to_string(),
            format!("invalid contract: skills '{SKILL_URN}' has no published version")
        );
    }

    #[test]
    fn floating_playbook_follows_the_same_owner_draft_rule() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        store
            .put_playbook_with_owner(&test_playbook(), "alice")
            .expect("release");
        store
            .publish_playbook_draft_with_owner(&test_playbook(), "alice")
            .expect("draft");
        let floating = unversioned_ref(PLAYBOOK_URN);
        let resolve = |owner: &str, mode| {
            resolve_ref_for_owner(&store, ArtifactKind::Playbook, &floating, owner, mode)
                .expect("resolve")
        };
        let draft = resolve("alice", ResolveMode::Draft);
        assert_eq!(draft.channel, SkillChannel::Draft);
        let release = resolve("alice", ResolveMode::Release);
        assert_eq!(release.channel, SkillChannel::Released);
        let foreign = resolve("bob", ResolveMode::Draft);
        assert_eq!(foreign.channel, SkillChannel::Released);
        assert_eq!(foreign.reference, release.reference);
    }

    #[test]
    fn default_and_allowed_playbook_refs_are_recorded_in_resolved_refs() {
        let store = store();
        store
            .put_playbook_with_owner(&test_playbook(), LOCAL_USER_ID)
            .expect("playbook");
        let mut agent = test_agent(vec![]);
        agent.allow_any_playbook = false;
        agent.default_playbook_ref = Some(unversioned_ref(PLAYBOOK_URN));
        agent.allowed_playbook_refs = vec![unversioned_ref(PLAYBOOK_URN)];
        store
            .put_agent_released_with_owner(&agent, LOCAL_USER_ID)
            .expect("agent");
        let mut spec = run_spec();
        spec.playbook_ref = None;
        record_resolved_refs(&store, &mut spec, &agent, LOCAL_USER_ID, false).expect("resolve");

        let recorded: Vec<_> = spec
            .resolved_refs
            .iter()
            .filter(|r| r.resource_id.as_ref().map(|id| id.urn.as_str()) == Some(PLAYBOOK_URN))
            .collect();
        assert_eq!(recorded.len(), 1, "default and allowed share one pin");
        assert!(recorded[0].version.is_some());
        assert!(recorded[0].digest.is_some());
    }

    #[test]
    fn allowed_playbook_ref_without_a_release_fails_closed() {
        let store = store();
        let mut agent = test_agent(vec![]);
        agent.allowed_playbook_refs = vec![unversioned_ref(PLAYBOOK_URN)];
        store
            .put_agent_released_with_owner(&agent, LOCAL_USER_ID)
            .expect("agent");
        let mut spec = run_spec();
        spec.playbook_ref = None;
        let err = record_resolved_refs(&store, &mut spec, &agent, LOCAL_USER_ID, false)
            .expect_err("no playbook");
        assert!(err.to_string().contains("no published version"), "{err}");
    }

    #[test]
    fn overlay_skill_additions_are_recorded_in_resolved_refs() {
        let store = store();
        store
            .put_skill_package(&package(DIGEST_100, 1, 0, 0, "", SkillChannel::Released))
            .expect("release");
        let mut agent = test_agent(vec![]);
        agent.overlay = Some(vam_spec::v1::AgentOverlay {
            skills: Some(vam_spec::v1::SkillOverlay {
                add: vec![unversioned_skill()],
                remove: vec![],
            }),
            ..Default::default()
        });
        store
            .put_agent_released_with_owner(&agent, LOCAL_USER_ID)
            .expect("agent");
        let mut spec = run_spec();
        spec.playbook_ref = None;
        record_resolved_refs(&store, &mut spec, &agent, LOCAL_USER_ID, false).expect("resolve");
        assert_eq!(
            pinned_skill(&spec).digest.as_ref().unwrap().hex_hash,
            DIGEST_100
        );
    }

    #[test]
    fn empty_sha256_placeholder_means_unpinned_through_the_resolver() {
        let store = store();
        store
            .put_skill_package(&package(DIGEST_100, 1, 0, 0, "", SkillChannel::Released))
            .expect("release");
        let mut versioned = unversioned_skill();
        versioned.version = Some(released_version());
        versioned.digest = Some(Digest {
            algorithm: SHA256,
            hex_hash: EMPTY_SHA256_HEX.to_string(),
        });
        let pinned = resolve_ref_for_owner(
            &store,
            ArtifactKind::Skill,
            &versioned,
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect("placeholder follows the catalog");
        assert_eq!(pinned.reference.digest.unwrap().hex_hash, DIGEST_100);
        assert_eq!(pinned.channel, SkillChannel::Released);
    }

    #[test]
    fn resolver_rejects_a_ref_of_another_kind() {
        let store = store();
        let err = resolve_ref_for_owner(
            &store,
            ArtifactKind::Playbook,
            &unversioned_skill(),
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect_err("kind mismatch");
        assert!(err.to_string().contains("not a playbook URN"), "{err}");
    }
    fn released_only(
        store: &SqliteWorkflowStore,
        kind: ArtifactKind,
        r: &ResourceRef,
        owner: &str,
    ) -> Result<PinnedRef, OrchestratorError> {
        resolve_ref_for_owner(store, kind, r, owner, ResolveMode::ReleasedOnly)
    }

    #[test]
    fn released_only_floating_ref_is_the_latest_release_never_the_owners_draft() {
        let store = store_with_alice_skill();
        let draft_mode = resolve_ref_for_owner(
            &store,
            ArtifactKind::Skill,
            &unversioned_skill(),
            "alice",
            ResolveMode::Draft,
        )
        .expect("draft mode");
        assert_eq!(draft_mode.channel, SkillChannel::Draft);
        let pinned = released_only(&store, ArtifactKind::Skill, &unversioned_skill(), "alice")
            .expect("released only");
        assert_eq!(pinned.channel, SkillChannel::Released);
        assert_eq!(pinned.reference.digest.unwrap().hex_hash, DIGEST_100);
    }

    #[test]
    fn released_only_floating_playbook_ignores_the_owners_draft() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store
            .put_playbook_with_owner(&test_playbook(), "alice")
            .expect("release");
        store
            .publish_playbook_draft_with_owner(&test_playbook(), "alice")
            .expect("draft");
        let pinned = released_only(
            &store,
            ArtifactKind::Playbook,
            &unversioned_ref(PLAYBOOK_URN),
            "alice",
        )
        .expect("released only");
        assert_eq!(pinned.channel, SkillChannel::Released);
    }

    #[test]
    fn released_only_floating_ref_with_only_a_draft_is_an_error() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store
            .put_skill_package_with_owner(
                &package(DIGEST_DRAFT, 1, 0, 0, "draft", SkillChannel::Draft),
                "alice",
            )
            .expect("draft only");
        released_only(&store, ArtifactKind::Skill, &unversioned_skill(), "alice")
            .expect_err("nothing released");
    }

    #[test]
    fn released_only_versioned_ref_is_that_release_or_an_error() {
        let store = store_with_alice_skill();
        let mut versioned = unversioned_skill();
        versioned.version = Some(released_version());
        let pinned = released_only(&store, ArtifactKind::Skill, &versioned, "bob")
            .expect("1.0.0 is released");
        assert_eq!(pinned.channel, SkillChannel::Released);

        // A version and digest the catalog does not hold: kept as-is in Release mode,
        // an error here.
        let mut absent = unversioned_skill();
        absent.version = Some(Version {
            major: 2,
            minor: 0,
            patch: 0,
            prerelease: String::new(),
            build: String::new(),
        });
        absent.digest = Some(Digest {
            algorithm: SHA256,
            hex_hash: DIGEST_200.to_string(),
        });
        let kept = resolve_ref_for_owner(
            &store,
            ArtifactKind::Skill,
            &absent,
            "bob",
            ResolveMode::Release,
        )
        .expect("kept as-is in Release mode");
        assert_eq!(kept.reference, absent);
        let err = released_only(&store, ArtifactKind::Skill, &absent, "bob")
            .expect_err("no release at 2.0.0");
        assert!(err.to_string().contains("not found in catalog"), "{err}");

        // A digest that is not the release's.
        let mut wrong = versioned.clone();
        wrong.digest = Some(Digest {
            algorithm: SHA256,
            hex_hash: DIGEST_200.to_string(),
        });
        let err =
            released_only(&store, ArtifactKind::Skill, &wrong, "bob").expect_err("digest mismatch");
        assert!(err.to_string().contains("digest mismatch"), "{err}");
    }

    #[test]
    fn released_only_refuses_an_explicit_draft_ref_even_for_the_owner_and_an_operator() {
        let store = store_with_alice_skill();
        let mut draft = unversioned_skill();
        draft.version = Some(draft_version());
        draft.digest = Some(Digest {
            algorithm: SHA256,
            hex_hash: DIGEST_DRAFT.to_string(),
        });
        resolve_ref_for_owner(
            &store,
            ArtifactKind::Skill,
            &draft,
            "alice",
            ResolveMode::Release,
        )
        .expect("Release mode still resolves the owner's own draft");
        let err = released_only(&store, ArtifactKind::Skill, &draft, "alice")
            .expect_err("own draft refused");
        assert!(
            err.to_string().contains("drafts are not available"),
            "{err}"
        );
        resolve_ref_visible(
            &store,
            ArtifactKind::Skill,
            &draft,
            "bob",
            ResolveMode::ReleasedOnly,
            true,
        )
        .expect_err("include_foreign_drafts is ignored");
        // A differently cased `-Draft` is refused too.
        let mut cased = draft.clone();
        if let Some(v) = cased.version.as_mut() {
            v.prerelease = "Draft".to_string();
        }
        released_only(&store, ArtifactKind::Skill, &cased, "alice").expect_err("cased draft");
    }

    // ---- Wave 2: a Member-owned draft agent runs released-only (MD-3.5) ----

    use crate::users::UserRole;

    fn demote(store: &SqliteWorkflowStore, user: &str) {
        store.set_user_role(user, UserRole::Member).expect("demote");
    }

    fn alice_draft_agent(store: &SqliteWorkflowStore, refs: Vec<ResourceRef>) -> AgentSpec {
        let agent = floating_agent(ALICE_AGENT_URN, refs);
        store
            .publish_agent_draft_with_owner(&agent, "alice")
            .expect("alice agent");
        agent
    }

    #[test]
    fn member_owned_draft_run_pins_the_latest_release_not_the_owners_draft() {
        let store = store_with_alice_skill();
        let agent = alice_draft_agent(&store, vec![unversioned_skill()]);
        let record = |store: &SqliteWorkflowStore| {
            let mut spec = draft_run_spec(ALICE_AGENT_URN);
            record_resolved_refs(store, &mut spec, &agent, "alice", false).expect("resolve");
            pinned_skill(&spec).clone()
        };
        // A publisher's draft run takes their draft.
        assert_eq!(record(&store).version.unwrap().prerelease, "draft");
        // The demoted publisher's draft run flips to the release.
        demote(&store, "alice");
        let pinned = record(&store);
        assert_eq!(pinned.version.unwrap().prerelease, "");
        assert_eq!(pinned.digest.unwrap().hex_hash, DIGEST_100);
    }

    #[test]
    fn member_owned_draft_run_refuses_unreleased_pins_also_for_an_admin_caller() {
        let store = store_with_alice_skill();
        demote(&store, "alice");
        let missing = ResourceRef {
            version: Some(Version {
                major: 9,
                minor: 9,
                patch: 9,
                prerelease: String::new(),
                build: String::new(),
            }),
            digest: Some(Digest {
                algorithm: SHA256,
                hex_hash: DIGEST_200.to_string(),
            }),
            ..unversioned_skill()
        };
        for skill_ref in [missing, draft_ref(SKILL_URN, DIGEST_DRAFT)] {
            let agent = alice_draft_agent(&store, vec![skill_ref]);
            for (caller, admin) in [("alice", false), ("bob", true)] {
                let mut spec = draft_run_spec(ALICE_AGENT_URN);
                let err = record_resolved_refs(&store, &mut spec, &agent, caller, admin)
                    .expect_err("refused");
                let text = err.to_string();
                assert!(
                    text.contains("not found in catalog")
                        || text.contains("drafts are not available"),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn a_floating_skill_with_no_release_says_released_for_a_member_owned_draft() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store
            .put_skill_package_with_owner(
                &package(DIGEST_DRAFT, 1, 0, 0, "draft", SkillChannel::Draft),
                "alice",
            )
            .expect("draft only");
        let agent = alice_draft_agent(&store, vec![unversioned_skill()]);
        let mut spec = draft_run_spec(ALICE_AGENT_URN);
        let err = record_resolved_refs(&store, &mut spec, &agent, "alice", false)
            .expect_err("no release");
        assert!(err.to_string().contains("has no released version"), "{err}");
    }

    #[test]
    fn who_authors_follows_the_live_role() {
        let store = store();
        store.create_user("pat", "pat").expect("pat");
        // `local` is a Member, a user with no row is not an author, the bootstrap owner always is.
        assert!(!owner_authors(&store, LOCAL_USER_ID));
        assert!(!owner_authors(&store, "nobody"));
        assert!(!owner_authors(&store, "pat"));
        assert!(owner_authors(&store, crate::users::OPERATOR_USER_ID));
        for role in [UserRole::Publisher, UserRole::OrgAdmin, UserRole::Operator] {
            store.set_user_role("pat", role).expect("role");
            assert!(owner_authors(&store, "pat"), "{role:?}");
        }
        assert_eq!(
            draft_mode_for_owner(&store, LOCAL_USER_ID),
            ResolveMode::ReleasedOnly
        );
        assert_eq!(draft_mode_for_owner(&store, "pat"), ResolveMode::Draft);
    }

    #[test]
    fn a_bootstrap_owned_draft_run_stays_draft() {
        let store = store();
        let spec = draft_run_spec(ALICE_AGENT_URN);
        store
            .publish_agent_draft_with_owner(
                &floating_agent(ALICE_AGENT_URN, vec![]),
                crate::users::OPERATOR_USER_ID,
            )
            .expect("operator draft");
        let (owner, mode) = run_resolve_context(&store, &spec, crate::users::OPERATOR_USER_ID);
        assert_eq!(owner, crate::users::OPERATOR_USER_ID);
        assert_eq!(mode, ResolveMode::Draft);
    }

    #[test]
    fn the_local_owners_draft_run_is_released_only() {
        let store = store();
        let spec = draft_run_spec(ALICE_AGENT_URN);
        store
            .publish_agent_draft_with_owner(&floating_agent(ALICE_AGENT_URN, vec![]), LOCAL_USER_ID)
            .expect("local draft");
        let (owner, mode) = run_resolve_context(&store, &spec, LOCAL_USER_ID);
        assert_eq!(owner, LOCAL_USER_ID);
        assert_eq!(mode, ResolveMode::ReleasedOnly);
    }

    #[test]
    fn a_members_run_of_a_released_agent_keeps_its_frozen_refs() {
        // A released agent runs as it was released: a versioned ref the catalog does not
        // hold (seed or HSP synthetic skill) is kept as-is for a Member caller.
        let store = store();
        store.create_user("mia", "mia").expect("member");
        let frozen = ResourceRef {
            version: Some(Version {
                major: 7,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            digest: Some(Digest {
                algorithm: SHA256,
                hex_hash: DIGEST_200.to_string(),
            }),
            ..unversioned_skill()
        };
        let agent = test_agent(vec![frozen.clone()]);
        store
            .put_agent_released_with_owner(&agent, LOCAL_USER_ID)
            .expect("released agent");
        let mut spec = RunSpec::default();
        record_resolved_refs(&store, &mut spec, &agent, "mia", false).expect("member run");
        assert_eq!(spec.resolved_refs, vec![frozen]);
    }

    #[test]
    fn a_members_explicit_draft_playbook_is_refused_even_on_their_own_draft() {
        let store = store();
        store.create_user("mia", "mia").expect("member");
        store
            .publish_playbook_draft_with_owner(&test_playbook(), "mia")
            .expect("mia draft");
        let version = draft_ref(PLAYBOOK_URN, "").version.expect("v");
        let digest = store
            .catalog_pin_at_version("playbooks", PLAYBOOK_URN, &version, "mia", false)
            .expect("pin")
            .digest;
        let mut spec = run_spec();
        spec.playbook_ref = Some(draft_ref(PLAYBOOK_URN, &digest));
        let err = record_resolved_refs(&store, &mut spec, &AgentSpec::default(), "mia", false)
            .expect_err("a member never runs a draft playbook");
        assert!(
            err.to_string().contains("drafts are not available"),
            "{err}"
        );
    }

    #[test]
    fn advanced_policy_refs_are_recorded_in_every_mode() {
        let store = store_with_alice_skill();
        store
            .put_policy_released_with_owner(
                POLICY_URN,
                "permit(principal, action, resource);",
                "alice",
            )
            .expect("policy release");
        let mut agent = floating_agent(ALICE_AGENT_URN, vec![]);
        agent.permissions = Some(vam_spec::v1::AgentPermissions {
            advanced: vec![unversioned_ref(POLICY_URN)],
            ..Default::default()
        });
        store
            .put_agent_released_with_owner(&agent, "alice")
            .expect("release");
        store
            .publish_agent_draft_with_owner(&agent, "alice")
            .expect("draft");
        let record = |draft: bool| {
            let mut spec = if draft {
                draft_run_spec(ALICE_AGENT_URN)
            } else {
                RunSpec::default()
            };
            record_resolved_refs(&store, &mut spec, &agent, "alice", false).expect("record");
            spec.resolved_refs
        };
        for draft in [false, true] {
            let refs = record(draft);
            assert_eq!(refs.len(), 1, "draft={draft}");
            assert!(refs[0].version.is_some() && refs[0].digest.is_some());
        }
        demote(&store, "alice");
        assert_eq!(record(true).len(), 1);
    }

    #[test]
    fn a_recorded_draft_pin_fails_closed_at_boot_once_released_only() {
        let spec = RunSpec {
            resolved_refs: vec![draft_ref(SKILL_URN, DIGEST_DRAFT)],
            ..Default::default()
        };
        require_no_draft_pins(&spec, ResolveMode::Draft).expect("draft mode");
        let err = require_no_draft_pins(&spec, ResolveMode::ReleasedOnly).expect_err("closed");
        assert!(
            err.to_string().contains("drafts are not available"),
            "{err}"
        );
    }
}
