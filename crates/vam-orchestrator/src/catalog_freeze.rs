//! Release freezes every catalog reference to an exact version and digest.
//!
//! A draft may carry floating refs (no version), placeholder digests or
//! `-draft` pins. A release must not: [`freeze_agent`] and [`freeze_playbook`]
//! resolve every ref through [`resolve_ref_for_owner`] in
//! [`ResolveMode::Release`] and write the pin back into the spec.
//!
//! A ref that resolves only to a draft, or to nothing released, cannot be
//! frozen. All such refs are collected and reported in one
//! [`OrchestratorError::DependenciesNotReleased`] (HTTP 409), each with the fix.
//! The 409 text stays the direct offenders, in first-seen order, including
//! evaluators. [`unreleased_dependencies`] is the structured list the release
//! suggestion returns: transitive, catalog artifacts only, in release order.
//!
//! The frozen bytes are what a release stores and digests. "Same content" checks
//! (the no-op release, the digest conflict, the release suggestion) therefore
//! compare `freeze(draft)` with the latest release, never the raw draft.

use std::collections::{HashMap, HashSet};

use prost::Message;
use sha2::{Digest as _, Sha256};
use vam_spec::v1::{
    AgentSpec, Digest, PlaybookSpec, ResourceRef, SkillChannel, UnreleasedDependency, Version,
};
use vam_spec::validation::Validate;

use crate::catalog_resolve::{
    is_missing_release_error, is_unpinned_digest, pinned_resource_ref, resolve_ref_for_owner,
    ArtifactKind, ResolveMode,
};
use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

/// A frozen spec's encoded bytes and digest, ready to store.
pub(crate) struct Frozen {
    pub bytes: Vec<u8>,
    pub digest: String,
}

/// Encode `spec`, unless it is equal to what the draft stored: then the draft's own
/// bytes and digest stand, so a spec with nothing to freeze keeps its digest.
fn encode<T: Message + PartialEq>(
    spec: T,
    original: &T,
    original_bytes: &[u8],
    original_digest: &str,
) -> Result<Frozen, OrchestratorError> {
    if &spec == original {
        return Ok(Frozen {
            bytes: original_bytes.to_vec(),
            digest: original_digest.to_string(),
        });
    }
    let mut bytes = Vec::new();
    spec.encode(&mut bytes)
        .map_err(|e| OrchestratorError::Core(e.to_string()))?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    Ok(Frozen { bytes, digest })
}

/// Freeze every ref of the stored draft `bytes` for a release by `owner`.
pub(crate) fn freeze_agent(
    store: &SqliteWorkflowStore,
    bytes: &[u8],
    digest: &str,
    owner: &str,
) -> Result<Frozen, OrchestratorError> {
    let original = AgentSpec::decode(bytes).map_err(|e| OrchestratorError::Core(e.to_string()))?;
    let mut agent = original.clone();
    let mut freezer = Freezer::new(store, owner, "agent", urn_of(agent.resource_id.as_ref()));

    for r in &mut agent.skill_refs {
        freezer.catalog(r)?;
    }
    if let Some(skills) = agent.overlay.as_mut().and_then(|o| o.skills.as_mut()) {
        for r in &mut skills.add {
            freezer.catalog(r)?;
        }
    }
    if let Some(r) = agent.default_playbook_ref.as_mut() {
        freezer.catalog(r)?;
    }
    for r in &mut agent.allowed_playbook_refs {
        freezer.catalog(r)?;
    }
    if let Some(r) = agent
        .capability_policy
        .as_mut()
        .and_then(|p| p.policy_ref.as_mut())
    {
        freezer.catalog(r)?;
    }
    if let Some(r) = agent
        .hitl_policy
        .as_mut()
        .and_then(|p| p.policy_ref.as_mut())
    {
        freezer.catalog(r)?;
    }
    if let Some(r) = agent
        .telemetry_policy
        .as_mut()
        .and_then(|p| p.policy_ref.as_mut())
    {
        freezer.catalog(r)?;
    }
    if let Some(completion) = agent.completion_policy.as_mut() {
        if let Some(r) = completion.policy_ref.as_mut() {
            freezer.catalog(r)?;
        }
        for r in &mut completion.evaluator_refs {
            freezer.evaluator(r);
        }
    }
    if let Some(r) = agent.extends.as_mut() {
        freezer.extends(r)?;
    }

    freezer.finish()?;
    agent
        .validate()
        .map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?;
    encode(agent, &original, bytes, digest)
}

/// Freeze `requires.skills` of the stored draft `bytes` for a release by `owner`.
pub(crate) fn freeze_playbook(
    store: &SqliteWorkflowStore,
    bytes: &[u8],
    digest: &str,
    owner: &str,
) -> Result<Frozen, OrchestratorError> {
    let original =
        PlaybookSpec::decode(bytes).map_err(|e| OrchestratorError::Core(e.to_string()))?;
    let mut playbook = original.clone();
    let mut freezer = Freezer::new(
        store,
        owner,
        "playbook",
        urn_of(playbook.resource_id.as_ref()),
    );
    if let Some(requires) = playbook.requires.as_mut() {
        for r in &mut requires.skills {
            freezer.catalog(r)?;
        }
    }
    freezer.finish()?;
    playbook
        .validate()
        .map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?;
    encode(playbook, &original, bytes, digest)
}

fn urn_of(id: Option<&vam_spec::v1::ResourceId>) -> String {
    id.map(|id| id.urn.clone()).unwrap_or_default()
}

fn ref_urn(r: &ResourceRef) -> &str {
    r.resource_id.as_ref().map_or("", |id| id.urn.as_str())
}

fn is_draft(version: Option<&Version>) -> bool {
    version.is_some_and(|v| v.prerelease == "draft")
}

fn version_label(v: &Version) -> String {
    format!("{}.{}.{}", v.major, v.minor, v.patch)
}

struct Freezer<'a> {
    store: &'a SqliteWorkflowStore,
    owner: &'a str,
    /// What is being released: "agent" or "playbook".
    noun: &'static str,
    /// URN of the artifact being released.
    urn: String,
    /// One line per dependency that cannot be frozen, in first-seen order.
    offenders: Vec<String>,
}

impl<'a> Freezer<'a> {
    fn new(
        store: &'a SqliteWorkflowStore,
        owner: &'a str,
        noun: &'static str,
        urn: String,
    ) -> Self {
        Self {
            store,
            owner,
            noun,
            urn,
            offenders: Vec::new(),
        }
    }

    fn offend(&mut self, line: String) {
        if !self.offenders.contains(&line) {
            self.offenders.push(line);
        }
    }

    fn fix(&self) -> String {
        format!("release it first, then release this {}.", self.noun)
    }

    fn not_released(&mut self, dep_noun: &str, r: &ResourceRef) {
        let urn = ref_urn(r).to_string();
        let fix = self.fix();
        let line = match r.version.as_ref() {
            None => format!("{dep_noun} '{urn}' has no released version: {fix}"),
            Some(v) if is_draft(Some(v)) => {
                format!("{dep_noun} '{urn}' is pinned to a draft, which cannot be released: {fix}")
            }
            Some(v) => format!(
                "{dep_noun} '{urn}' {} is not released: {fix}",
                version_label(v)
            ),
        };
        self.offend(line);
    }

    /// Freeze a skill, playbook or policy ref in place. An unset ref (no URN) is
    /// an absent optional field and is left alone.
    fn catalog(&mut self, r: &mut ResourceRef) -> Result<(), OrchestratorError> {
        let urn = ref_urn(r).to_string();
        if urn.is_empty() {
            return Ok(());
        }
        let kind = ArtifactKind::from_urn(&urn)?;
        match resolve_ref_for_owner(self.store, kind, r, self.owner, ResolveMode::Release) {
            Ok(pinned) if pinned.channel == SkillChannel::Draft => {
                self.not_released(dep_noun(kind), r);
            }
            Ok(pinned) => *r = pinned.reference,
            Err(err) if is_missing_release_error(&err) => self.not_released(dep_noun(kind), r),
            Err(err) => return Err(err),
        }
        Ok(())
    }

    /// Freeze `extends`, an agent ref. The resolver does not cover agents, so
    /// this uses the same catalog pin lookups it is built on.
    fn extends(&mut self, r: &mut ResourceRef) -> Result<(), OrchestratorError> {
        let urn = ref_urn(r).to_string();
        if urn.is_empty() {
            return Ok(());
        }
        if is_draft(r.version.as_ref()) {
            self.not_released("agent", r);
            return Ok(());
        }
        let pin = match r.version.as_ref() {
            None => self.store.latest_released_catalog_pin("agents", &urn),
            Some(v) => self
                .store
                .catalog_pin_at_version("agents", &urn, v, self.owner, false),
        };
        let pin = match pin {
            Ok(pin) => pin,
            Err(OrchestratorError::NotFound(_)) => {
                self.not_released("agent", r);
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        if let Some(digest) = r.digest.as_ref() {
            if r.version.is_some()
                && !is_unpinned_digest(&digest.hex_hash)
                && digest.hex_hash != pin.digest
            {
                return Err(OrchestratorError::InvalidContract(format!(
                    "ref '{urn}' digest mismatch: pin {}, catalog {}",
                    digest.hex_hash, pin.digest
                )));
            }
        }
        *r = pinned_resource_ref(&urn, &pin, std::mem::take(&mut r.locators));
        Ok(())
    }

    /// Evaluator refs name no catalog kind yet, so nothing can resolve them. A
    /// release keeps one only if it already carries a version and a real digest.
    fn evaluator(&mut self, r: &ResourceRef) {
        let urn = ref_urn(r);
        if urn.is_empty() {
            return;
        }
        let pinned = r.version.is_some()
            && !is_draft(r.version.as_ref())
            && r.digest
                .as_ref()
                .is_some_and(|d: &Digest| !is_unpinned_digest(&d.hex_hash));
        if !pinned {
            self.offend(format!(
                "evaluator '{urn}' cannot be resolved from the catalog: pin it with an explicit version and digest, then release this {}.",
                self.noun
            ));
        }
    }

    fn finish(self) -> Result<(), OrchestratorError> {
        if self.offenders.is_empty() {
            return Ok(());
        }
        let n = self.offenders.len();
        let mut lines = vec![format!(
            "cannot release {} '{}': {n} {} not released",
            self.noun,
            self.urn,
            if n == 1 {
                "dependency is"
            } else {
                "dependencies are"
            }
        )];
        lines.extend(self.offenders.into_iter().map(|l| format!("  {l}")));
        Err(OrchestratorError::DependenciesNotReleased(lines))
    }
}

fn dep_noun(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Skill => "skill",
        ArtifactKind::Playbook => "playbook",
        ArtifactKind::Policy => "policy",
    }
}

/// Which draft the suggestion is about. Skills, packages and policies carry no
/// catalog refs, so they have nothing to list.
#[derive(Clone, Copy)]
pub(crate) enum FreezeSubject {
    Agent,
    Playbook,
}

/// Draft-only dependencies of `bytes`, in an order where releasing each in turn
/// succeeds.
///
/// Order is skills (`skill_package` and `skill`), then playbooks, policies, and
/// `extends` agents. A dependency follows the draft-only needs of its own draft
/// when the caller owns that draft. The same URN is listed once. Evaluator refs
/// are omitted. This does not change the release 409.
pub(crate) fn unreleased_dependencies(
    store: &SqliteWorkflowStore,
    subject: FreezeSubject,
    bytes: &[u8],
    owner: &str,
) -> Result<Vec<UnreleasedDependency>, OrchestratorError> {
    let mut found = Found::new(store, owner);
    match subject {
        FreezeSubject::Agent => {
            let agent =
                AgentSpec::decode(bytes).map_err(|e| OrchestratorError::Core(e.to_string()))?;
            found.walk_agent(&agent, None)?;
        }
        FreezeSubject::Playbook => {
            let playbook =
                PlaybookSpec::decode(bytes).map_err(|e| OrchestratorError::Core(e.to_string()))?;
            found.walk_playbook(&playbook, None)?;
        }
    }
    Ok(found.finish())
}

/// `child` must be released before `parent`.
type NeedEdge = (String, String);

struct Node {
    dep: UnreleasedDependency,
    rank: u8,
    discovery: usize,
}

struct Found<'a> {
    store: &'a SqliteWorkflowStore,
    owner: &'a str,
    nodes: HashMap<String, Node>,
    /// `(need, dependent)`: the need is released first.
    edges: Vec<NeedEdge>,
    stack: HashSet<String>,
    next_discovery: usize,
}

impl<'a> Found<'a> {
    fn new(store: &'a SqliteWorkflowStore, owner: &'a str) -> Self {
        Self {
            store,
            owner,
            nodes: HashMap::new(),
            edges: Vec::new(),
            stack: HashSet::new(),
            next_discovery: 0,
        }
    }

    fn walk_agent(
        &mut self,
        agent: &AgentSpec,
        parent: Option<&str>,
    ) -> Result<(), OrchestratorError> {
        for r in &agent.skill_refs {
            self.discover(r, parent)?;
        }
        if let Some(skills) = agent.overlay.as_ref().and_then(|o| o.skills.as_ref()) {
            for r in &skills.add {
                self.discover(r, parent)?;
            }
        }
        if let Some(r) = agent.default_playbook_ref.as_ref() {
            self.discover(r, parent)?;
        }
        for r in &agent.allowed_playbook_refs {
            self.discover(r, parent)?;
        }
        for r in [
            agent
                .capability_policy
                .as_ref()
                .and_then(|p| p.policy_ref.as_ref()),
            agent
                .hitl_policy
                .as_ref()
                .and_then(|p| p.policy_ref.as_ref()),
            agent
                .telemetry_policy
                .as_ref()
                .and_then(|p| p.policy_ref.as_ref()),
            agent
                .completion_policy
                .as_ref()
                .and_then(|p| p.policy_ref.as_ref()),
        ]
        .into_iter()
        .flatten()
        {
            self.discover(r, parent)?;
        }
        if let Some(r) = agent.extends.as_ref() {
            self.discover(r, parent)?;
        }
        Ok(())
    }

    fn walk_playbook(
        &mut self,
        playbook: &PlaybookSpec,
        parent: Option<&str>,
    ) -> Result<(), OrchestratorError> {
        if let Some(requires) = playbook.requires.as_ref() {
            for r in &requires.skills {
                self.discover(r, parent)?;
            }
        }
        Ok(())
    }

    fn discover(&mut self, r: &ResourceRef, parent: Option<&str>) -> Result<(), OrchestratorError> {
        let urn = ref_urn(r).to_string();
        if urn.is_empty() {
            return Ok(());
        }
        if self.nodes.contains_key(&urn) {
            self.edge(parent, &urn);
            return Ok(());
        }
        if self.stack.contains(&urn) {
            return Ok(());
        }
        if !self.is_unreleased(r)? {
            return Ok(());
        }
        let (kind, can_release) = self.classify(&urn)?;
        self.stack.insert(urn.clone());
        if can_release {
            self.walk_owned_needs(&urn, kind)?;
        }
        self.stack.remove(&urn);
        if self.nodes.contains_key(&urn) {
            self.edge(parent, &urn);
            return Ok(());
        }
        let discovery = self.next_discovery;
        self.next_discovery += 1;
        self.nodes.insert(
            urn.clone(),
            Node {
                dep: UnreleasedDependency {
                    kind: kind.to_string(),
                    urn: urn.clone(),
                    reason: reason_code(r).to_string(),
                    caller_can_release: can_release,
                },
                rank: kind_rank(kind),
                discovery,
            },
        );
        self.edge(parent, &urn);
        Ok(())
    }

    /// Needs of a draft the caller owns. A foreign draft is invisible, so its
    /// requirements are not listed from here.
    fn walk_owned_needs(&mut self, urn: &str, kind: &str) -> Result<(), OrchestratorError> {
        let pin = draft_pin();
        match kind {
            "playbook" => {
                let spec = match self
                    .store
                    .get_playbook_at_version(urn, &pin, self.owner, false)
                {
                    Ok(spec) => spec,
                    Err(OrchestratorError::NotFound(_)) => return Ok(()),
                    Err(err) => return Err(err),
                };
                self.walk_playbook(&spec, Some(urn))?;
            }
            "agent" => {
                let spec = match self
                    .store
                    .get_agent_at_version_visible(urn, &pin, self.owner, false)
                {
                    Ok(spec) => spec,
                    Err(OrchestratorError::NotFound(_)) => return Ok(()),
                    Err(err) => return Err(err),
                };
                self.walk_agent(&spec, Some(urn))?;
            }
            _ => {}
        }
        Ok(())
    }

    fn edge(&mut self, parent: Option<&str>, child: &str) {
        let Some(parent) = parent else {
            return;
        };
        let edge = (child.to_string(), parent.to_string());
        if !self.edges.contains(&edge) {
            self.edges.push(edge);
        }
    }

    fn is_unreleased(&self, r: &ResourceRef) -> Result<bool, OrchestratorError> {
        let urn = ref_urn(r);
        match urn_kind(urn) {
            Some("agent") => extends_unreleased(self.store, self.owner, r),
            Some("skill" | "playbook" | "policy") => catalog_unreleased(self.store, self.owner, r),
            Some(other) => Err(OrchestratorError::InvalidContract(format!(
                "cannot resolve catalog kind '{other}' for '{urn}'"
            ))),
            None => Err(OrchestratorError::InvalidContract(format!(
                "unsupported catalog URN '{urn}'"
            ))),
        }
    }

    fn classify(&self, urn: &str) -> Result<(&'static str, bool), OrchestratorError> {
        match urn_kind(urn) {
            Some("agent") => Ok(("agent", self.owns_table("agents", urn)?)),
            Some("playbook") => Ok(("playbook", self.owns_table("playbooks", urn)?)),
            Some("policy") => Ok(("policy", self.owns_table("policies", urn)?)),
            Some("skill") => self.skill_kind(urn),
            Some(other) => Err(OrchestratorError::InvalidContract(format!(
                "cannot resolve catalog kind '{other}' for '{urn}'"
            ))),
            None => Err(OrchestratorError::InvalidContract(format!(
                "unsupported catalog URN '{urn}'"
            ))),
        }
    }

    /// `skill_package` when a package draft or release row exists, otherwise
    /// `skill`. The caller can release it only when they own that row's draft.
    fn skill_kind(&self, urn: &str) -> Result<(&'static str, bool), OrchestratorError> {
        let owns_package = self.owns_package(urn)?;
        if owns_package || self.package_row_exists(urn)? {
            return Ok(("skill_package", owns_package));
        }
        Ok(("skill", self.owns_table("skills", urn)?))
    }

    fn owns_package(&self, urn: &str) -> Result<bool, OrchestratorError> {
        match self.store.get_skill_package_for_query_visible(
            urn,
            None,
            SkillChannel::Draft,
            self.owner,
            false,
        ) {
            Ok(_) => Ok(true),
            Err(OrchestratorError::NotFound(_)) => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn package_row_exists(&self, urn: &str) -> Result<bool, OrchestratorError> {
        if package_visible(
            self.store
                .get_skill_package_for_query(urn, None, SkillChannel::Draft),
        )? {
            return Ok(true);
        }
        package_visible(
            self.store
                .get_skill_package_for_query(urn, None, SkillChannel::Released),
        )
    }

    fn owns_table(&self, table: &str, urn: &str) -> Result<bool, OrchestratorError> {
        match self
            .store
            .catalog_pin_at_version(table, urn, &draft_pin(), self.owner, false)
        {
            Ok(_) => Ok(true),
            Err(OrchestratorError::NotFound(_)) => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn finish(self) -> Vec<UnreleasedDependency> {
        let mut indegree: HashMap<String, usize> = HashMap::new();
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        for (need, dependent) in &self.edges {
            if !self.nodes.contains_key(need) || !self.nodes.contains_key(dependent) {
                continue;
            }
            *indegree.entry(dependent.clone()).or_insert(0) += 1;
            dependents
                .entry(need.clone())
                .or_default()
                .push(dependent.clone());
        }
        let mut remaining: HashSet<String> = self.nodes.keys().cloned().collect();
        let mut out = Vec::with_capacity(remaining.len());
        while !remaining.is_empty() {
            let ready = remaining
                .iter()
                .filter(|urn| indegree.get(*urn).copied().unwrap_or(0) == 0)
                .min_by_key(|urn| node_order(&self.nodes, urn))
                .cloned();
            let Some(urn) = ready.or_else(|| {
                remaining
                    .iter()
                    .min_by_key(|urn| node_order(&self.nodes, urn))
                    .cloned()
            }) else {
                break;
            };
            remaining.remove(&urn);
            if let Some(waiting) = dependents.get(&urn) {
                for dependent in waiting {
                    if let Some(degree) = indegree.get_mut(dependent) {
                        *degree = degree.saturating_sub(1);
                    }
                }
            }
            if let Some(node) = self.nodes.get(&urn) {
                out.push(node.dep.clone());
            }
        }
        out
    }
}

fn node_order(nodes: &HashMap<String, Node>, urn: &str) -> (u8, usize) {
    nodes
        .get(urn)
        .map(|node| (node.rank, node.discovery))
        .unwrap_or((u8::MAX, usize::MAX))
}

fn kind_rank(kind: &str) -> u8 {
    match kind {
        "skill" | "skill_package" => 0,
        "playbook" => 1,
        "policy" => 2,
        "agent" => 3,
        _ => 4,
    }
}

/// The three offender cases in [`Freezer::not_released`].
fn reason_code(r: &ResourceRef) -> &'static str {
    match r.version.as_ref() {
        None => "no_release",
        Some(v) if is_draft(Some(v)) => "pinned_to_draft",
        Some(_) => "version_not_released",
    }
}

fn draft_pin() -> Version {
    Version {
        prerelease: "draft".to_string(),
        ..Default::default()
    }
}

fn urn_kind(urn: &str) -> Option<&str> {
    urn.strip_prefix("urn:vam:")?.split(':').next()
}

fn package_visible(
    found: Result<vam_spec::v1::SkillPackageRecord, OrchestratorError>,
) -> Result<bool, OrchestratorError> {
    match found {
        Ok(_) => Ok(true),
        Err(OrchestratorError::NotFound(_)) => Ok(false),
        Err(err) => Err(err),
    }
}

/// Same decision as [`Freezer::catalog`], without writing the pin.
fn catalog_unreleased(
    store: &SqliteWorkflowStore,
    owner: &str,
    r: &ResourceRef,
) -> Result<bool, OrchestratorError> {
    let urn = ref_urn(r).to_string();
    if urn.is_empty() {
        return Ok(false);
    }
    let kind = ArtifactKind::from_urn(&urn)?;
    match resolve_ref_for_owner(store, kind, r, owner, ResolveMode::Release) {
        Ok(pinned) => Ok(pinned.channel == SkillChannel::Draft),
        Err(err) if is_missing_release_error(&err) => Ok(true),
        Err(err) => Err(err),
    }
}

/// Same decision as [`Freezer::extends`], without writing the pin.
fn extends_unreleased(
    store: &SqliteWorkflowStore,
    owner: &str,
    r: &ResourceRef,
) -> Result<bool, OrchestratorError> {
    let urn = ref_urn(r);
    if urn.is_empty() {
        return Ok(false);
    }
    if is_draft(r.version.as_ref()) {
        return Ok(true);
    }
    let pin = match r.version.as_ref() {
        None => store.latest_released_catalog_pin("agents", urn),
        Some(v) => store.catalog_pin_at_version("agents", urn, v, owner, false),
    };
    match pin {
        Ok(pin) => {
            if let Some(digest) = r.digest.as_ref() {
                if r.version.is_some()
                    && !is_unpinned_digest(&digest.hex_hash)
                    && digest.hex_hash != pin.digest
                {
                    return Err(OrchestratorError::InvalidContract(format!(
                        "ref '{urn}' digest mismatch: pin {}, catalog {}",
                        digest.hex_hash, pin.digest
                    )));
                }
            }
            Ok(false)
        }
        Err(OrchestratorError::NotFound(_)) => Ok(true),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_suggest::SuggestKind;
    use crate::users::LOCAL_USER_ID;
    use vam_spec::v1::{
        agent_spec::InstructionSource, AgentOverlay, CapabilityPolicyRef, CompletionPolicyRef,
        PlaybookRequirements, PlaybookStep, ResourceId, SkillChannel, SkillManifest, SkillOverlay,
        SkillPackageRecord, StepMode,
    };

    const SKILL: &str = "urn:vam:skill:local.vam.dev/acme/jira";
    const SKILL_B: &str = "urn:vam:skill:local.vam.dev/acme/slack";
    const AGENT: &str = "urn:vam:agent:local.vam.dev/acme/freezer";
    const PARENT: &str = "urn:vam:agent:local.vam.dev/acme/parent";
    const PLAYBOOK: &str = "urn:vam:playbook:local.vam.dev/acme/pb";
    const POLICY: &str = "urn:vam:policy:local.vam.dev/acme/pol";
    const CEDAR: &str = "permit(principal, action, resource);";

    fn v(major: i32, minor: i32, patch: i32) -> Version {
        Version {
            major,
            minor,
            patch,
            ..Default::default()
        }
    }

    fn floating(urn: &str) -> ResourceRef {
        ResourceRef {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            ..Default::default()
        }
    }

    fn agent(urn: &str, skills: &[&str]) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: "Freezer".to_string(),
            description: "test".to_string(),
            role: "tester".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions("go".into())),
            allow_any_playbook: true,
            skill_refs: skills.iter().map(|u| floating(u)).collect(),
            ..Default::default()
        }
    }

    fn playbook(requires: &[&str]) -> PlaybookSpec {
        PlaybookSpec {
            activation: Some(vam_spec::v1::PlaybookActivation::asking(
                "Use when the request matches this test playbook.",
            )),
            resource_id: Some(ResourceId {
                urn: PLAYBOOK.to_string(),
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
            requires: Some(PlaybookRequirements {
                skills: requires.iter().map(|u| floating(u)).collect(),
                capabilities: vec![],
            }),
            ..Default::default()
        }
    }

    /// Release `urn` as `major.minor.patch` with a description, returning its digest.
    fn release_skill(store: &SqliteWorkflowStore, urn: &str, ver: Version, text: &str) -> String {
        let manifest = SkillManifest {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            version: Some(ver.clone()),
            description: text.to_string(),
            ..Default::default()
        };
        store
            .publish_skill_draft_with_owner(&manifest, LOCAL_USER_ID)
            .expect("skill draft");
        store.release_skill(urn, &ver).expect("release skill");
        store
            .catalog_pin_at_version("skills", urn, &ver, LOCAL_USER_ID, false)
            .expect("skill pin")
            .digest
    }

    fn draft_skill(store: &SqliteWorkflowStore, urn: &str) {
        let manifest = SkillManifest {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            version: Some(v(1, 0, 0)),
            description: "draft only".to_string(),
            ..Default::default()
        };
        store
            .publish_skill_draft_with_owner(&manifest, LOCAL_USER_ID)
            .expect("skill draft");
    }

    fn released_agent(store: &SqliteWorkflowStore, urn: &str, ver: Version) -> AgentSpec {
        store.release_agent(urn, &ver).expect("release agent")
    }

    fn pin_of<'a>(refs: &'a [ResourceRef], urn: &str) -> &'a ResourceRef {
        refs.iter()
            .find(|r| ref_urn(r) == urn)
            .unwrap_or_else(|| panic!("no ref for {urn}"))
    }

    #[test]
    fn floating_skill_ref_is_frozen_to_latest_release_and_digest() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_skill(&store, SKILL, v(1, 0, 0), "one");
        let digest = release_skill(&store, SKILL, v(1, 1, 0), "two");
        store
            .publish_agent_draft_with_owner(&agent(AGENT, &[SKILL]), LOCAL_USER_ID)
            .expect("draft");

        let released = released_agent(&store, AGENT, v(1, 0, 0));

        let skill = pin_of(&released.skill_refs, SKILL);
        let version = skill.version.as_ref().expect("version");
        assert_eq!((version.major, version.minor, version.patch), (1, 1, 0));
        assert_eq!(version.prerelease, "");
        assert_eq!(skill.digest.as_ref().expect("digest").hex_hash, digest);
        released.validate().expect("frozen spec validates");

        // The stored row is the frozen one, and the draft is still floating.
        let stored = store.get_agent(AGENT).expect("stored");
        assert_eq!(stored.skill_refs, released.skill_refs);
        let draft_bytes = store
            .draft_for_freeze(crate::catalog::CatalogKind::Agent, AGENT)
            .expect("draft row");
        let draft = AgentSpec::decode(draft_bytes.spec_bytes.as_slice()).expect("decode");
        assert!(draft.skill_refs[0].version.is_none());
    }

    #[test]
    fn every_agent_ref_slot_is_frozen() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let skill_digest = release_skill(&store, SKILL, v(1, 0, 0), "skill");
        let other_digest = release_skill(&store, SKILL_B, v(2, 0, 0), "other");
        store
            .publish_playbook_draft_with_owner(&playbook(&[]), LOCAL_USER_ID)
            .expect("pb draft");
        store.release_playbook(PLAYBOOK, &v(1, 0, 0)).expect("pb");
        store
            .publish_policy_draft_with_owner(POLICY, CEDAR, LOCAL_USER_ID)
            .expect("policy draft");
        store.release_policy(POLICY, &v(1, 0, 0)).expect("policy");
        store
            .publish_agent_draft_with_owner(&agent(PARENT, &[SKILL]), LOCAL_USER_ID)
            .expect("parent draft");
        let parent = released_agent(&store, PARENT, v(1, 0, 0));
        let parent_digest = store
            .catalog_pin_at_version("agents", PARENT, &v(1, 0, 0), LOCAL_USER_ID, false)
            .expect("parent pin")
            .digest;
        assert_eq!(parent.skill_refs.len(), 1);

        let mut child = agent(AGENT, &[]);
        child.instruction_source = None;
        child.extends = Some(floating(PARENT));
        child.overlay = Some(AgentOverlay {
            skills: Some(SkillOverlay {
                add: vec![floating(SKILL_B)],
                remove: vec![],
            }),
            ..Default::default()
        });
        child.default_playbook_ref = Some(floating(PLAYBOOK));
        child.allowed_playbook_refs = vec![floating(PLAYBOOK)];
        child.allow_any_playbook = false;
        child.capability_policy = Some(CapabilityPolicyRef {
            policy_ref: Some(floating(POLICY)),
            security_profile_name: String::new(),
        });
        store
            .publish_agent_draft_with_owner(&child, LOCAL_USER_ID)
            .expect("child draft");

        let released = released_agent(&store, AGENT, v(1, 0, 0));

        let extends = released.extends.as_ref().expect("extends");
        assert_eq!(extends.version, Some(v(1, 0, 0)));
        assert_eq!(extends.digest.as_ref().unwrap().hex_hash, parent_digest);
        let add = &released
            .overlay
            .as_ref()
            .unwrap()
            .skills
            .as_ref()
            .unwrap()
            .add;
        assert_eq!(add[0].version, Some(v(2, 0, 0)));
        assert_eq!(add[0].digest.as_ref().unwrap().hex_hash, other_digest);
        let default = released.default_playbook_ref.as_ref().expect("default");
        assert_eq!(default.version, Some(v(1, 0, 0)));
        assert!(default.digest.is_some());
        assert_eq!(released.allowed_playbook_refs[0].version, Some(v(1, 0, 0)));
        let policy = released
            .capability_policy
            .as_ref()
            .and_then(|p| p.policy_ref.as_ref())
            .expect("policy ref");
        assert_eq!(policy.version, Some(v(1, 0, 0)));
        assert!(policy.digest.is_some());
        released.validate().expect("frozen spec validates");
        let _ = skill_digest;
    }

    #[test]
    fn playbook_requires_skills_are_frozen() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let digest = release_skill(&store, SKILL, v(1, 3, 0), "skill");
        store
            .publish_playbook_draft_with_owner(&playbook(&[SKILL]), LOCAL_USER_ID)
            .expect("draft");
        let released = store
            .release_playbook(PLAYBOOK, &v(1, 0, 0))
            .expect("release");
        let skill = pin_of(&released.requires.as_ref().unwrap().skills, SKILL);
        assert_eq!(skill.version, Some(v(1, 3, 0)));
        assert_eq!(skill.digest.as_ref().unwrap().hex_hash, digest);
    }

    #[test]
    fn draft_only_and_missing_dependencies_are_all_listed_and_nothing_is_released() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        draft_skill(&store, SKILL); // draft only
                                    // SKILL_B has nothing at all.
        let mut spec = agent(AGENT, &[SKILL, SKILL_B]);
        spec.default_playbook_ref = Some(floating(PLAYBOOK));
        spec.allow_any_playbook = true;
        store
            .publish_agent_draft_with_owner(&spec, LOCAL_USER_ID)
            .expect("draft");

        let err = store
            .release_agent(AGENT, &v(1, 0, 0))
            .expect_err("must refuse");
        let OrchestratorError::DependenciesNotReleased(lines) = &err else {
            panic!("wrong error: {err:?}");
        };
        let text = err.to_string();
        assert_eq!(lines.len(), 4, "{text}"); // header + 3 offenders
        for urn in [SKILL, SKILL_B, PLAYBOOK] {
            assert!(
                text.contains(&format!(
                    "'{urn}' has no released version: release it first, then release this agent."
                )),
                "{text}"
            );
        }
        assert!(text.contains("3 dependencies are not released"), "{text}");
        let nothing =
            store.catalog_pin_at_version("agents", AGENT, &v(1, 0, 0), LOCAL_USER_ID, false);
        assert!(matches!(nothing, Err(OrchestratorError::NotFound(_))));

        // Releasing the dependencies first makes the same draft releasable.
        release_skill(&store, SKILL, v(1, 0, 0), "now released");
        release_skill(&store, SKILL_B, v(1, 0, 0), "slack");
        store
            .publish_playbook_draft_with_owner(&playbook(&[]), LOCAL_USER_ID)
            .expect("pb");
        store.release_playbook(PLAYBOOK, &v(1, 0, 0)).expect("pb");
        released_agent(&store, AGENT, v(1, 0, 0));
    }

    #[test]
    fn explicit_draft_pin_is_refused() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        draft_skill(&store, SKILL);
        let mut spec = agent(AGENT, &[]);
        let mut pinned = floating(SKILL);
        pinned.version = Some(Version {
            major: 1,
            prerelease: "draft".to_string(),
            ..Default::default()
        });
        spec.skill_refs = vec![pinned];
        store
            .publish_agent_draft_with_owner(&spec, LOCAL_USER_ID)
            .expect("draft");
        let err = store.release_agent(AGENT, &v(1, 0, 0)).expect_err("refuse");
        assert!(err.to_string().contains("is pinned to a draft"), "{err}");
    }

    #[test]
    fn releasing_twice_with_no_change_is_a_no_op() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_skill(&store, SKILL, v(1, 0, 0), "one");
        store
            .publish_agent_draft_with_owner(&agent(AGENT, &[SKILL]), LOCAL_USER_ID)
            .expect("draft");
        let first = released_agent(&store, AGENT, v(1, 0, 0));
        let again = released_agent(&store, AGENT, v(1, 0, 0));
        assert_eq!(first, again);

        let suggestion = store
            .release_suggestion(SuggestKind::Agent, AGENT)
            .expect("suggestion");
        assert!(suggestion.unchanged, "{suggestion:?}");
        assert_eq!(suggestion.bump, vam_spec::v1::ReleaseBump::None as i32);
    }

    #[test]
    fn dependency_release_since_last_release_is_a_patch_change() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_skill(&store, SKILL, v(1, 0, 0), "one");
        store
            .publish_agent_draft_with_owner(&agent(AGENT, &[SKILL]), LOCAL_USER_ID)
            .expect("draft");
        released_agent(&store, AGENT, v(1, 0, 0));

        release_skill(&store, SKILL, v(1, 0, 1), "two");

        let suggestion = store
            .release_suggestion(SuggestKind::Agent, AGENT)
            .expect("suggestion");
        assert!(!suggestion.unchanged, "{suggestion:?}");
        assert_eq!(suggestion.bump, vam_spec::v1::ReleaseBump::Patch as i32);
        assert_eq!(suggestion.reasons, vec!["updated skill jira to v1.0.1"]);
        assert_eq!(suggestion.suggested, Some(v(1, 0, 1)));

        // The moved dependency makes 1.0.0 a different release.
        let err = store
            .release_agent(AGENT, &v(1, 0, 0))
            .expect_err("conflict");
        assert!(matches!(err, OrchestratorError::Conflict(_)), "{err}");
        let next = released_agent(&store, AGENT, v(1, 0, 1));
        assert_eq!(pin_of(&next.skill_refs, SKILL).version, Some(v(1, 0, 1)));
    }

    #[test]
    fn legacy_put_path_freezes_and_refuses_too() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let err = store
            .put_agent_with_owner(&agent(AGENT, &[SKILL]), LOCAL_USER_ID)
            .expect_err("unreleased skill");
        assert!(
            matches!(err, OrchestratorError::DependenciesNotReleased(_)),
            "{err}"
        );
        let err = store
            .put_playbook_with_owner(&playbook(&[SKILL]), LOCAL_USER_ID)
            .expect_err("unreleased skill");
        assert!(
            matches!(err, OrchestratorError::DependenciesNotReleased(_)),
            "{err}"
        );

        let digest = release_skill(&store, SKILL, v(1, 0, 0), "one");
        let put = store
            .put_agent_with_owner(&agent(AGENT, &[SKILL]), LOCAL_USER_ID)
            .expect("put");
        let skill = pin_of(&put.skill_refs, SKILL);
        assert_eq!(skill.digest.as_ref().unwrap().hex_hash, digest);
    }

    const PACKAGE: &str = "urn:vam:skill:local.vam.dev/acme/jirapkg";
    const NEEDED: &str = "urn:vam:skill:local.vam.dev/acme/needed";
    const DIGEST_PKG: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn draft_package(urn: &str) -> SkillPackageRecord {
        SkillPackageRecord {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            version: Some(Version {
                major: 1,
                prerelease: "draft".to_string(),
                ..Default::default()
            }),
            channel: SkillChannel::Draft as i32,
            digest: DIGEST_PKG.to_string(),
            blob_relpath: "pending".to_string(),
            size_bytes: 8,
            spec: Some(SkillManifest {
                resource_id: Some(ResourceId {
                    urn: urn.to_string(),
                }),
                version: Some(Version {
                    major: 1,
                    prerelease: "draft".to_string(),
                    ..Default::default()
                }),
                description: "package draft".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn publish_package(store: &SqliteWorkflowStore, urn: &str, owner: &str) {
        store
            .put_skill_package_with_owner(&draft_package(urn), owner)
            .expect("package draft");
    }

    fn deps_of(store: &SqliteWorkflowStore, urn: &str) -> Vec<vam_spec::v1::UnreleasedDependency> {
        store
            .release_suggestion(SuggestKind::Agent, urn)
            .expect("suggestion")
            .unreleased_dependencies
    }

    #[test]
    fn suggestion_lists_draft_only_skills_before_their_playbook() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        publish_package(&store, PACKAGE, LOCAL_USER_ID);
        draft_skill(&store, NEEDED);
        store
            .publish_playbook_draft_with_owner(&playbook(&[NEEDED]), LOCAL_USER_ID)
            .expect("playbook draft");
        let mut spec = agent(AGENT, &[PACKAGE]);
        spec.default_playbook_ref = Some(floating(PLAYBOOK));
        store
            .publish_agent_draft_with_owner(&spec, LOCAL_USER_ID)
            .expect("agent draft");

        let deps = deps_of(&store, AGENT);
        assert_eq!(deps.len(), 3, "{deps:?}");
        assert_eq!(deps[0].kind, "skill_package");
        assert_eq!(deps[0].urn, PACKAGE);
        assert_eq!(deps[1].kind, "skill");
        assert_eq!(deps[1].urn, NEEDED);
        assert_eq!(deps[2].kind, "playbook");
        assert_eq!(deps[2].urn, PLAYBOOK);
        for dep in &deps {
            assert_eq!(dep.reason, "no_release");
            assert!(dep.caller_can_release, "{dep:?}");
        }
        let playbook_at = deps.iter().position(|d| d.urn == PLAYBOOK).unwrap();
        assert!(deps.iter().take(playbook_at).all(|d| d.kind != "playbook"));

        let playbook_suggestion = store
            .release_suggestion(SuggestKind::Playbook, PLAYBOOK)
            .expect("playbook suggestion");
        assert_eq!(playbook_suggestion.unreleased_dependencies.len(), 1);
        assert_eq!(playbook_suggestion.unreleased_dependencies[0].urn, NEEDED);
        assert_eq!(playbook_suggestion.unreleased_dependencies[0].kind, "skill");

        let suggestion = store
            .release_suggestion(SuggestKind::Agent, AGENT)
            .expect("suggestion");
        let json = vam_spec::encoding::proto_to_json("vam.v1.ReleaseSuggestion", &suggestion)
            .expect("json");
        assert!(
            json.contains("\"unreleasedDependencies\""),
            "JSON name must be unreleasedDependencies, got {json}"
        );
        assert!(
            json.contains("\"callerCanRelease\":true"),
            "JSON name must be callerCanRelease, got {json}"
        );

        // Releasing each dependency in turn makes the list empty. The 409 text
        // on a release that still has them stays the direct-offender message.
        let blocked = store.release_agent(AGENT, &v(1, 0, 0)).expect_err("409");
        let text = blocked.to_string();
        assert!(
            text.contains(&format!(
                "skill '{PACKAGE}' has no released version: release it first, then release this agent."
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "playbook '{PLAYBOOK}' has no released version: release it first, then release this agent."
            )),
            "{text}"
        );
        assert!(
            !text.contains(NEEDED),
            "transitive skill stays off the 409, got {text}"
        );

        store
            .release_skill_package(PACKAGE, &v(1, 0, 0))
            .expect("release package");
        release_skill(&store, NEEDED, v(1, 0, 0), "needed");
        store
            .release_playbook(PLAYBOOK, &v(1, 0, 0))
            .expect("release playbook");
        assert!(
            deps_of(&store, AGENT).is_empty(),
            "every dependency released"
        );
    }

    #[test]
    fn floating_ref_with_a_release_is_not_listed_when_a_draft_also_exists() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_skill(&store, SKILL, v(1, 0, 0), "released");
        draft_skill(&store, SKILL);
        store
            .publish_agent_draft_with_owner(&agent(AGENT, &[SKILL]), LOCAL_USER_ID)
            .expect("draft");
        assert!(
            deps_of(&store, AGENT).is_empty(),
            "a release wins over the owner's draft"
        );
    }

    #[test]
    fn foreign_draft_is_listed_with_caller_can_release_false() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("bob", "bob").expect("bob");
        publish_package(&store, PACKAGE, "bob");
        store
            .publish_agent_draft_with_owner(&agent(AGENT, &[PACKAGE]), LOCAL_USER_ID)
            .expect("agent draft");

        let deps = deps_of(&store, AGENT);
        assert_eq!(deps.len(), 1, "{deps:?}");
        assert_eq!(deps[0].kind, "skill_package");
        assert_eq!(deps[0].urn, PACKAGE);
        assert_eq!(deps[0].reason, "no_release");
        assert!(!deps[0].caller_can_release);
    }

    #[test]
    fn duplicate_skill_is_listed_once_and_reasons_match_the_ref() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        draft_skill(&store, SKILL);
        store
            .publish_playbook_draft_with_owner(&playbook(&[SKILL]), LOCAL_USER_ID)
            .expect("playbook");
        let mut spec = agent(AGENT, &[SKILL]);
        spec.default_playbook_ref = Some(floating(PLAYBOOK));
        store
            .publish_agent_draft_with_owner(&spec, LOCAL_USER_ID)
            .expect("agent");

        let deps = deps_of(&store, AGENT);
        assert_eq!(deps.len(), 2, "{deps:?}");
        assert_eq!(deps[0].urn, SKILL);
        assert_eq!(deps[1].urn, PLAYBOOK);

        let mut pinned = floating(SKILL_B);
        pinned.version = Some(Version {
            major: 1,
            prerelease: "draft".to_string(),
            ..Default::default()
        });
        let mut missing = floating(NEEDED);
        missing.version = Some(v(9, 9, 9));
        let mut spec = agent(AGENT, &[]);
        spec.skill_refs = vec![pinned, missing];
        store
            .publish_agent_draft_with_owner(&spec, LOCAL_USER_ID)
            .expect("replace draft");
        let deps = deps_of(&store, AGENT);
        assert_eq!(deps.len(), 2, "{deps:?}");
        assert_eq!(deps[0].urn, SKILL_B);
        assert_eq!(deps[0].reason, "pinned_to_draft");
        assert_eq!(deps[1].urn, NEEDED);
        assert_eq!(deps[1].reason, "version_not_released");
        assert!(!deps[0].caller_can_release);
        assert!(!deps[1].caller_can_release);
    }

    #[test]
    fn evaluator_offenders_are_not_listed() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let mut spec = agent(AGENT, &[]);
        spec.completion_policy = Some(CompletionPolicyRef {
            evaluator_refs: vec![floating("urn:vam:evaluator:local.vam.dev/acme/check")],
            ..Default::default()
        });
        store
            .publish_agent_draft_with_owner(&spec, LOCAL_USER_ID)
            .expect("draft");
        assert!(deps_of(&store, AGENT).is_empty());
        let err = store.release_agent(AGENT, &v(1, 0, 0)).expect_err("409");
        let text = err.to_string();
        assert!(text.contains("evaluator"), "{text}");
        assert!(matches!(err, OrchestratorError::DependenciesNotReleased(_)));
    }
}
