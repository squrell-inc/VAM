//! Release-version suggestion: compares the caller's draft with the latest
//! release and proposes the next semver (design/agent-experience.md §II.4).
//!
//! One pure `classify_*` function per kind returns the highest matching level
//! and every matching reason. [`build_suggestion`] turns that into the wire
//! message. Where a rule is ambiguous for a real spec field, the higher bump
//! wins; those calls are named on the function that makes them.

use std::collections::{BTreeMap, BTreeSet};

use prost::Message;
use rusqlite::{params, OptionalExtension};
use vam_spec::decision_strictness;
use vam_spec::permissions::{HostPattern, Specificity, Strictness, Target, ToolSelector};
use vam_spec::v1::{
    AgentPermissions, AgentSpec, HostDefault, PermissionChange, PermissionChangeKind,
    PermissionDecision, PermissionRule, PlaybookSpec, PlaybookStep, ReleaseBump, ReleaseSuggestion,
    ResourceRef, SkillManifest, ToolDefault, UnreleasedDependency, Version,
};

use crate::catalog_publish::{release_no_draft_error, ReleaseTriple};
use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

/// What the suggestion endpoint compares. The skill kind and the skill package
/// kind share one rule function because both carry a `SkillManifest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestKind {
    Agent,
    Playbook,
    Skill,
    SkillPackage,
    Policy,
}

impl SuggestKind {
    fn table(self) -> &'static str {
        match self {
            Self::Agent => "agents",
            Self::Playbook => "playbooks",
            Self::Skill => "skills",
            Self::SkillPackage => "skill_packages",
            Self::Policy => "policies",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Playbook => "playbook",
            Self::Skill | Self::SkillPackage => "skill",
            Self::Policy => "policy",
        }
    }
}

/// Order is bump order: the highest level among matched rules wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Patch,
    Minor,
    Major,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    pub level: Level,
    pub reasons: Vec<String>,
    /// Agent permission diffs. Empty for other kinds and when the effective
    /// permissions did not change.
    pub permission_changes: Vec<PermissionChange>,
}

/// Collects matches and resolves them to the highest level.
#[derive(Default)]
struct Matches {
    level: Option<Level>,
    reasons: Vec<String>,
    permission_changes: Vec<PermissionChange>,
}

impl Matches {
    fn add(&mut self, level: Level, reason: impl Into<String>) {
        self.level = Some(self.level.map_or(level, |l| l.max(level)));
        self.reasons.push(reason.into());
    }

    /// A draft whose digest differs but matched no rule is still a change.
    fn finish(mut self) -> Classification {
        if self.level.is_none() {
            self.add(Level::Patch, "other changes");
        }
        Classification {
            level: self.level.unwrap_or(Level::Patch),
            reasons: self.reasons,
            permission_changes: self.permission_changes,
        }
    }

    fn add_permission(&mut self, level: Level, change: PermissionChange) {
        self.add(level, change.sentence.clone());
        self.permission_changes.push(change);
    }
}

fn urn_name(urn: &str) -> &str {
    urn.rsplit('/').next().unwrap_or(urn)
}

fn ref_urn(r: &ResourceRef) -> &str {
    r.resource_id.as_ref().map_or("", |id| id.urn.as_str())
}

fn urn_set<'a>(refs: impl IntoIterator<Item = &'a ResourceRef>) -> BTreeSet<&'a str> {
    refs.into_iter()
        .map(ref_urn)
        .filter(|u| !u.is_empty())
        .collect()
}

/// Added and removed members of two sets, in sorted order.
fn set_delta<'a>(old: &BTreeSet<&'a str>, new: &BTreeSet<&'a str>) -> (Vec<&'a str>, Vec<&'a str>) {
    (
        new.difference(old).copied().collect(),
        old.difference(new).copied().collect(),
    )
}

// ---------------------------------------------------------------- agent

/// Agent rules:
/// - MAJOR: skill ref removed; playbook removed from `allowed_playbook_refs`
///   or `default_playbook_ref`; `allow_any_playbook` true→false; capability
///   policy ref changed to a different URN; `extends` changed; a permission
///   target's effective decision becomes `deny`.
/// - MINOR: skill ref added; playbook added; `allow_any_playbook` false→true;
///   any other permission change (loosening, `allow` or `bySideEffect` → `ask`,
///   an `advanced` ref added or removed, legacy Cedar → rules).
/// - PATCH: anything else.
///
/// Conservative calls: a capability policy ref added or cleared counts as
/// "changed to a different URN", except when that clear is the legacy →
/// permissions move; any change to the `extends` ref (URN, version or digest)
/// counts as changed, because it changes the inherited spec; a default
/// playbook replaced by a different one removes the old default.
///
/// Permission sentences are also returned on [`Classification::permission_changes`].
/// The highest level across these rules and the permission rules wins.
pub fn classify_agent(old: &AgentSpec, new: &AgentSpec) -> Classification {
    let mut m = Matches::default();

    let old_skills = urn_set(&old.skill_refs);
    let new_skills = urn_set(&new.skill_refs);
    let (added, removed) = set_delta(&old_skills, &new_skills);
    for urn in removed {
        m.add(Level::Major, format!("removed skill {}", urn_name(urn)));
    }
    for urn in added {
        m.add(Level::Minor, format!("added skill {}", urn_name(urn)));
    }

    let old_allowed = urn_set(&old.allowed_playbook_refs);
    let new_allowed = urn_set(&new.allowed_playbook_refs);
    let old_default = old.default_playbook_ref.as_ref().map_or("", ref_urn);
    let new_default = new.default_playbook_ref.as_ref().map_or("", ref_urn);
    let mut old_all = old_allowed.clone();
    let mut new_all = new_allowed.clone();
    old_all.extend(Some(old_default).filter(|u| !u.is_empty()));
    new_all.extend(Some(new_default).filter(|u| !u.is_empty()));

    let (_, removed_allowed) = set_delta(&old_allowed, &new_allowed);
    for urn in removed_allowed {
        m.add(Level::Major, format!("removed playbook {}", urn_name(urn)));
    }
    if !old_default.is_empty() && old_default != new_default {
        m.add(
            Level::Major,
            format!("removed default playbook {}", urn_name(old_default)),
        );
    }
    let (added_playbooks, _) = set_delta(&old_all, &new_all);
    for urn in added_playbooks {
        m.add(Level::Minor, format!("added playbook {}", urn_name(urn)));
    }

    match (old.allow_any_playbook, new.allow_any_playbook) {
        (true, false) => m.add(Level::Major, "allowAnyPlaybook turned off"),
        (false, true) => m.add(Level::Minor, "allowAnyPlaybook turned on"),
        _ => {}
    }

    // Clearing `capability_policy` because the draft adopted `permissions` is
    // the legacy move below, not a separate policy swap.
    let migrating = legacy_to_permissions(old, new);
    if capability_policy_urn(old) != capability_policy_urn(new)
        && !(migrating && capability_policy_urn(new).is_empty())
    {
        m.add(Level::Major, "changed capability policy");
    }

    if old.extends != new.extends {
        m.add(Level::Major, "changed extends");
    }

    if old.instruction_source != new.instruction_source {
        m.add(Level::Patch, "edited instructions");
    }
    if old.display_name != new.display_name {
        m.add(Level::Patch, "edited display name");
    }
    if old.description != new.description {
        m.add(Level::Patch, "edited description");
    }
    if old.role != new.role {
        m.add(Level::Patch, "edited role");
    }
    if old.model_requirements != new.model_requirements
        || old.preferred_models != new.preferred_models
    {
        m.add(Level::Patch, "changed model settings");
    }
    if old.context_policy != new.context_policy || old.reasoning_policy != new.reasoning_policy {
        m.add(Level::Patch, "changed context settings");
    }
    pin_changes(&mut m, "skill", &old.skill_refs, &new.skill_refs);
    pin_changes(
        &mut m,
        "playbook",
        &old.allowed_playbook_refs,
        &new.allowed_playbook_refs,
    );
    pin_changes(
        &mut m,
        "playbook",
        old.default_playbook_ref.as_slice(),
        new.default_playbook_ref.as_slice(),
    );
    pin_changes(
        &mut m,
        "skill",
        overlay_skill_refs(old),
        overlay_skill_refs(new),
    );
    pin_changes(&mut m, "policy", &policy_refs(old), &policy_refs(new));
    pin_changes(&mut m, "policy", &advanced_refs(old), &advanced_refs(new));
    permission_diff(old, new, &mut m);

    m.finish()
}

fn capability_policy_urn(agent: &AgentSpec) -> String {
    agent
        .capability_policy
        .as_ref()
        .and_then(|c| c.policy_ref.as_ref())
        .map_or(String::new(), |r| ref_urn(r).to_string())
}

/// The previous release still has a capability policy and the draft has rules.
fn legacy_to_permissions(old: &AgentSpec, new: &AgentSpec) -> bool {
    !capability_policy_urn(old).is_empty() && old.permissions.is_none() && new.permissions.is_some()
}

const LEGACY_PERMISSIONS_SENTENCE: &str =
    "Permissions moved from a Cedar policy to rules; review the effective view";

/// Effective decision inside the agent scope, including the tool-default rank
/// `bySideEffect`.
struct Effect {
    label: &'static str,
    /// Higher is stricter (`deny > ask > bySideEffect > allow`).
    rank: Strictness,
}

fn permission_diff(old: &AgentSpec, new: &AgentSpec, m: &mut Matches) {
    if legacy_to_permissions(old, new) {
        m.add_permission(
            Level::Minor,
            PermissionChange {
                target: "permissions".to_string(),
                before: "cedar".to_string(),
                after: "rules".to_string(),
                change: PermissionChangeKind::Added as i32,
                sentence: LEGACY_PERMISSIONS_SENTENCE.to_string(),
            },
        );
        return;
    }

    let old_perms = old.permissions.clone().unwrap_or_default();
    let new_perms = new.permissions.clone().unwrap_or_default();

    push_decision(
        m,
        "tools.default",
        tool_default_effect(&old_perms),
        tool_default_effect(&new_perms),
    );
    let mut tool_targets = indexed_targets(tool_rules(&old_perms), true);
    tool_targets.extend(indexed_targets(tool_rules(&new_perms), true));
    for (name, target) in tool_targets {
        push_decision(
            m,
            &name,
            tool_target_effect(&old_perms, &target),
            tool_target_effect(&new_perms, &target),
        );
    }

    push_decision(
        m,
        "internet.default",
        host_default_effect(&old_perms),
        host_default_effect(&new_perms),
    );
    let mut host_targets = indexed_targets(host_rules(&old_perms), false);
    host_targets.extend(indexed_targets(host_rules(&new_perms), false));
    for (name, target) in host_targets {
        push_decision(
            m,
            &name,
            host_target_effect(&old_perms, &target),
            host_target_effect(&new_perms, &target),
        );
    }

    // Web access (AR-1.7): turning it on loosens the agent (minor); turning it
    // off removes tools its callers may rely on (major, like a move to deny).
    let (old_web, new_web) = (
        crate::web_admission::agent_web_enabled(old),
        crate::web_admission::agent_web_enabled(new),
    );
    if old_web != new_web {
        let (level, kind, sentence) = if new_web {
            (
                Level::Minor,
                PermissionChangeKind::Loosened,
                "Now has web search and fetch: `web`",
            )
        } else {
            (
                Level::Major,
                PermissionChangeKind::Tightened,
                "Web search and fetch are now off: `web`",
            )
        };
        let word = |on: bool| if on { "on" } else { "off" };
        m.add_permission(
            level,
            PermissionChange {
                target: "web".to_string(),
                before: word(old_web).to_string(),
                after: word(new_web).to_string(),
                change: kind as i32,
                sentence: sentence.to_string(),
            },
        );
    }

    let old_advanced = advanced_urns(old);
    let new_advanced = advanced_urns(new);
    for urn in new_advanced.difference(&old_advanced) {
        m.add_permission(
            Level::Minor,
            PermissionChange {
                target: urn.clone(),
                before: String::new(),
                after: urn.clone(),
                change: PermissionChangeKind::Added as i32,
                sentence: format!("Added advanced policy `{urn}`"),
            },
        );
    }
    for urn in old_advanced.difference(&new_advanced) {
        m.add_permission(
            Level::Minor,
            PermissionChange {
                target: urn.clone(),
                before: urn.clone(),
                after: String::new(),
                change: PermissionChangeKind::Removed as i32,
                sentence: format!("Removed advanced policy `{urn}`"),
            },
        );
    }
}

fn push_decision(m: &mut Matches, target: &str, before: Effect, after: Effect) {
    if before.label == after.label {
        return;
    }
    let kind = if after.rank < before.rank {
        PermissionChangeKind::Loosened
    } else {
        PermissionChangeKind::Tightened
    };
    // Any move to deny is major. Every other effective change is at least
    // minor (allow → ask, bySideEffect → ask, and any loosening).
    let level = if after.label == "deny" {
        Level::Major
    } else {
        Level::Minor
    };
    m.add_permission(
        level,
        PermissionChange {
            target: target.to_string(),
            before: before.label.to_string(),
            after: after.label.to_string(),
            change: kind as i32,
            sentence: decision_sentence(target, before.label, after.label),
        },
    );
}

fn decision_sentence(target: &str, before: &str, after: &str) -> String {
    match after {
        "allow" => format!("Now allowed without asking: `{target}` (was: {before})"),
        "deny" => format!("Now denied: `{target}`"),
        "ask" => format!("Now asks: `{target}` (was: {before})"),
        _ => format!("Now decided by side effect: `{target}` (was: {before})"),
    }
}

fn tool_rules(perms: &AgentPermissions) -> &[PermissionRule] {
    perms
        .tools
        .as_ref()
        .map(|tools| tools.rules.as_slice())
        .unwrap_or(&[])
}

fn host_rules(perms: &AgentPermissions) -> &[PermissionRule] {
    perms
        .internet
        .as_ref()
        .map(|internet| internet.rules.as_slice())
        .unwrap_or(&[])
}

fn indexed_targets(rules: &[PermissionRule], want_tool: bool) -> BTreeMap<String, Target> {
    let mut out = BTreeMap::new();
    for rule in rules {
        let Ok(target) = Target::parse(&rule.target) else {
            continue;
        };
        if target.is_tool() != want_tool {
            continue;
        }
        out.insert(target.to_string(), target);
    }
    out
}

fn tool_default_effect(perms: &AgentPermissions) -> Effect {
    let defaults = perms.effective_defaults();
    Effect {
        label: match defaults.tools {
            ToolDefault::Allow => "allow",
            ToolDefault::Ask => "ask",
            ToolDefault::Deny => "deny",
            ToolDefault::BySideEffect | ToolDefault::Unspecified => "bySideEffect",
        },
        rank: defaults.tools_strictness(),
    }
}

fn host_default_effect(perms: &AgentPermissions) -> Effect {
    let defaults = perms.effective_defaults();
    Effect {
        label: match defaults.internet {
            HostDefault::Deny => "deny",
            HostDefault::Allow | HostDefault::Unspecified => "allow",
        },
        rank: defaults.internet_strictness(),
    }
}

fn tool_target_effect(perms: &AgentPermissions, target: &Target) -> Effect {
    if let Some(decision) = winning_decision(tool_rules(perms), target) {
        if let Some(rank) = decision_strictness(decision) {
            return Effect {
                label: decision_label(decision),
                rank,
            };
        }
    }
    tool_default_effect(perms)
}

fn host_target_effect(perms: &AgentPermissions, target: &Target) -> Effect {
    if let Some(decision) = winning_decision(host_rules(perms), target) {
        if let Some(rank) = decision_strictness(decision) {
            return Effect {
                label: decision_label(decision),
                rank,
            };
        }
    }
    host_default_effect(perms)
}

fn decision_label(decision: PermissionDecision) -> &'static str {
    match decision {
        PermissionDecision::Allow => "allow",
        PermissionDecision::Ask => "ask",
        PermissionDecision::Deny => "deny",
        PermissionDecision::Unspecified => "allow",
    }
}

/// Most specific matching rule in this list. A later rule of equal specificity wins.
fn winning_decision(rules: &[PermissionRule], subject: &Target) -> Option<PermissionDecision> {
    let mut best: Option<(Specificity, PermissionDecision)> = None;
    for rule in rules {
        let Ok(rule_target) = Target::parse(&rule.target) else {
            continue;
        };
        if !rule_covers(&rule_target, subject) {
            continue;
        }
        let Ok(decision) = PermissionDecision::try_from(rule.decision) else {
            continue;
        };
        if decision_strictness(decision).is_none() {
            continue;
        }
        let spec = rule_target.specificity();
        let replace = match &best {
            None => true,
            Some((prev, _)) => spec
                .partial_cmp(prev)
                .is_some_and(|order| order != std::cmp::Ordering::Less),
        };
        if replace {
            best = Some((spec, decision));
        }
    }
    best.map(|(_, decision)| decision)
}

/// Whether `rule` applies to `subject` inside one agent scope.
fn rule_covers(rule: &Target, subject: &Target) -> bool {
    if rule == subject {
        return true;
    }
    match (rule, subject) {
        (Target::ToolsAll, subject) if subject.is_tool() => true,
        (Target::InternetAll, subject) if subject.is_host() => true,
        (
            Target::Skill {
                name: rule_name,
                tool: ToolSelector::All,
            },
            Target::Skill {
                name: subject_name,
                tool: ToolSelector::Exact(_),
            },
        ) => rule_name == subject_name,
        (
            Target::Mcp {
                server: rule_server,
                tool: ToolSelector::All,
            },
            Target::Mcp {
                server: subject_server,
                tool: ToolSelector::Exact(_),
            },
        ) => rule_server == subject_server,
        (Target::Host(rule_host), Target::Host(subject_host)) => {
            host_pattern_covers(rule_host, subject_host)
        }
        _ => false,
    }
}

fn host_pattern_covers(rule: &HostPattern, subject: &HostPattern) -> bool {
    match (rule, subject) {
        (HostPattern::Wildcard { suffix }, HostPattern::Exact(host)) => {
            vam_spec::match_host(&format!("*.{suffix}"), host)
        }
        (HostPattern::Wildcard { suffix: broad }, HostPattern::Wildcard { suffix: narrow }) => {
            broader_wildcard_covers(broad, narrow)
        }
        (HostPattern::Exact(_), _) => false,
    }
}

/// `*.com` covers `*.pastebin.com`. Equal patterns are handled by [`rule_covers`].
fn broader_wildcard_covers(broad: &str, narrow: &str) -> bool {
    let Some(dot) = narrow.len().checked_sub(broad.len().saturating_add(1)) else {
        return false;
    };
    narrow.as_bytes().get(dot) == Some(&b'.') && narrow.ends_with(broad)
}

fn advanced_urns(agent: &AgentSpec) -> BTreeSet<String> {
    agent
        .permissions
        .as_ref()
        .map(|perms| {
            perms
                .advanced
                .iter()
                .map(|r| ref_urn(r).to_string())
                .filter(|urn| !urn.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn advanced_refs(agent: &AgentSpec) -> Vec<ResourceRef> {
    agent
        .permissions
        .as_ref()
        .map(|perms| perms.advanced.clone())
        .unwrap_or_default()
}

fn overlay_skill_refs(agent: &AgentSpec) -> &[ResourceRef] {
    agent
        .overlay
        .as_ref()
        .and_then(|o| o.skills.as_ref())
        .map_or(&[], |s| s.add.as_slice())
}

/// The four policy refs, cloned so they can sit in one slice.
fn policy_refs(agent: &AgentSpec) -> Vec<ResourceRef> {
    [
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
    .cloned()
    .collect()
}

/// A ref kept at the same URN whose version or digest moved is a PATCH.
///
/// Both sides are frozen (see `catalog_freeze`), so a moved pin means the
/// dependency has a newer release: "updated skill search to v1.2.0".
fn pin_changes(m: &mut Matches, noun: &str, old: &[ResourceRef], new: &[ResourceRef]) {
    let old_by_urn: BTreeMap<&str, &ResourceRef> = old.iter().map(|r| (ref_urn(r), r)).collect();
    for r in new {
        if let Some(prev) = old_by_urn.get(ref_urn(r)) {
            if prev.version != r.version || prev.digest != r.digest {
                let name = urn_name(ref_urn(r));
                m.add(
                    Level::Patch,
                    match r.version.as_ref() {
                        Some(v) => format!(
                            "updated {noun} {name} to v{}.{}.{}",
                            v.major, v.minor, v.patch
                        ),
                        None => format!("updated {noun} {name} pin"),
                    },
                );
            }
        }
    }
}

// ------------------------------------------------------------- playbook

fn struct_field<'a>(
    s: Option<&'a prost_types::Struct>,
    key: &str,
) -> Option<&'a prost_types::Value> {
    s.and_then(|s| s.fields.get(key))
}

fn string_list(value: Option<&prost_types::Value>) -> BTreeSet<String> {
    match value.and_then(|v| v.kind.as_ref()) {
        Some(prost_types::value::Kind::ListValue(list)) => list
            .values
            .iter()
            .filter_map(|v| match &v.kind {
                Some(prost_types::value::Kind::StringValue(s)) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => BTreeSet::new(),
    }
}

fn property_names(schema: Option<&prost_types::Struct>) -> BTreeSet<String> {
    match struct_field(schema, "properties").and_then(|v| v.kind.as_ref()) {
        Some(prost_types::value::Kind::StructValue(props)) => {
            props.fields.keys().cloned().collect()
        }
        _ => BTreeSet::new(),
    }
}

fn gate_options(step: &PlaybookStep) -> BTreeSet<&str> {
    step.options.iter().map(|o| o.label.as_str()).collect()
}

/// Playbook rules:
/// - MAJOR: step removed; input added to `inputSchema.required`; gate option
///   removed.
/// - MINOR: step added; optional input added.
/// - PATCH: anything else.
///
/// Conservative call: an existing optional input that becomes required counts
/// as "added to required". Steps are matched by id.
pub fn classify_playbook(old: &PlaybookSpec, new: &PlaybookSpec) -> Classification {
    let mut m = Matches::default();

    let old_steps: BTreeMap<&str, &PlaybookStep> =
        old.steps.iter().map(|s| (s.id.as_str(), s)).collect();
    let new_steps: BTreeMap<&str, &PlaybookStep> =
        new.steps.iter().map(|s| (s.id.as_str(), s)).collect();
    for id in old_steps.keys().filter(|id| !new_steps.contains_key(*id)) {
        m.add(Level::Major, format!("removed step {id}"));
    }
    for id in new_steps.keys().filter(|id| !old_steps.contains_key(*id)) {
        m.add(Level::Minor, format!("added step {id}"));
    }
    for (id, old_step) in &old_steps {
        let Some(new_step) = new_steps.get(id) else {
            continue;
        };
        let kept = gate_options(new_step);
        for label in gate_options(old_step).difference(&kept) {
            m.add(
                Level::Major,
                format!("removed option {label} from gate {id}"),
            );
        }
    }

    let old_required = string_list(struct_field(old.input_schema.as_ref(), "required"));
    let new_required = string_list(struct_field(new.input_schema.as_ref(), "required"));
    for name in new_required.difference(&old_required) {
        m.add(Level::Major, format!("added required input {name}"));
    }
    let old_props = property_names(old.input_schema.as_ref());
    for name in property_names(new.input_schema.as_ref()).difference(&old_props) {
        if !new_required.contains(name) {
            m.add(Level::Minor, format!("added optional input {name}"));
        }
    }

    if old.display_name != new.display_name {
        m.add(Level::Patch, "edited display name");
    }
    if old.description != new.description {
        m.add(Level::Patch, "edited description");
    }
    let step_text_changed = new_steps.iter().any(|(id, s)| {
        old_steps
            .get(id)
            .is_some_and(|o| o.instruction != s.instruction || o.description != s.description)
    });
    if step_text_changed {
        m.add(Level::Patch, "edited step instructions");
    }
    pin_changes(
        &mut m,
        "skill",
        vam_spec::required_skill_refs(old),
        vam_spec::required_skill_refs(new),
    );

    m.finish()
}

// ---------------------------------------------------------------- skill

/// Skill and skill package rules:
/// - MAJOR: capability removed from the declared capabilities; tool removed.
/// - MINOR: capability added; tool added.
/// - PATCH: anything else.
pub fn classify_skill(old: &SkillManifest, new: &SkillManifest) -> Classification {
    let mut m = Matches::default();

    let old_caps: BTreeSet<&str> = old.capabilities.iter().map(String::as_str).collect();
    let new_caps: BTreeSet<&str> = new.capabilities.iter().map(String::as_str).collect();
    let (added, removed) = set_delta(&old_caps, &new_caps);
    for cap in removed {
        m.add(Level::Major, format!("removed capability {cap}"));
    }
    for cap in added {
        m.add(Level::Minor, format!("added capability {cap}"));
    }

    let old_tools: BTreeSet<&str> = old.tools.iter().map(|t| t.name.as_str()).collect();
    let new_tools: BTreeSet<&str> = new.tools.iter().map(|t| t.name.as_str()).collect();
    let (added, removed) = set_delta(&old_tools, &new_tools);
    for tool in removed {
        m.add(Level::Major, format!("removed tool {tool}"));
    }
    for tool in added {
        m.add(Level::Minor, format!("added tool {tool}"));
    }

    if old.description != new.description {
        m.add(Level::Patch, "edited description");
    }
    if old.system_extension != new.system_extension {
        m.add(Level::Patch, "edited instructions");
    }

    m.finish()
}

// --------------------------------------------------------------- policy

/// Policy rule: any Cedar change is MINOR. Cedar cannot be classified
/// reliably, so the reason says the call is conservative.
pub fn classify_policy(old_cedar: &str, new_cedar: &str) -> Classification {
    let mut m = Matches::default();
    if old_cedar != new_cedar {
        m.add(
            Level::Minor,
            "Cedar changed; Cedar cannot be classified reliably, so this is a conservative minor bump",
        );
    }
    m.finish()
}

// ------------------------------------------------------------- assemble

/// Everything [`build_suggestion`] needs about the two sides of the compare.
pub struct Compared {
    pub latest: Option<ReleaseTriple>,
    pub latest_digest: String,
    pub draft_digest: String,
    /// Only for the skill package kind: the draft manifest's own version.
    pub manifest_version: Option<ReleaseTriple>,
    pub classification: Option<Classification>,
}

fn version(t: ReleaseTriple) -> Version {
    Version {
        major: t.0,
        minor: t.1,
        patch: t.2,
        prerelease: String::new(),
        build: String::new(),
    }
}

/// Next version for a bump level over `latest`.
fn next_triple(latest: ReleaseTriple, bump: Option<Level>) -> ReleaseTriple {
    let (major, minor, patch) = latest;
    match bump {
        Some(Level::Major) => (major.saturating_add(1), 0, 0),
        Some(Level::Minor) => (major, minor.saturating_add(1), 0),
        Some(Level::Patch) | None => (major, minor, patch.saturating_add(1)),
    }
}

pub fn build_suggestion(c: Compared) -> ReleaseSuggestion {
    let Some(latest) = c.latest else {
        let initial = c
            .manifest_version
            .filter(|v| *v > (0, 0, 0))
            .unwrap_or((1, 0, 0));
        return ReleaseSuggestion {
            latest: None,
            suggested: Some(version(initial)),
            bump: ReleaseBump::Initial as i32,
            reasons: vec!["first release".to_string()],
            unchanged: false,
            permission_changes: Vec::new(),
            unreleased_dependencies: Vec::new(),
        };
    };
    if c.latest_digest == c.draft_digest {
        return ReleaseSuggestion {
            latest: Some(version(latest)),
            suggested: Some(version(next_triple(latest, None))),
            bump: ReleaseBump::None as i32,
            reasons: Vec::new(),
            unchanged: true,
            permission_changes: Vec::new(),
            unreleased_dependencies: Vec::new(),
        };
    }
    let classification = c.classification.unwrap_or_else(|| Classification {
        level: Level::Patch,
        reasons: vec!["other changes".to_string()],
        permission_changes: Vec::new(),
    });
    let bump = match classification.level {
        Level::Patch => ReleaseBump::Patch,
        Level::Minor => ReleaseBump::Minor,
        Level::Major => ReleaseBump::Major,
    };
    ReleaseSuggestion {
        latest: Some(version(latest)),
        suggested: Some(version(next_triple(latest, Some(classification.level)))),
        bump: bump as i32,
        reasons: classification.reasons,
        unchanged: false,
        permission_changes: classification.permission_changes,
        unreleased_dependencies: Vec::new(),
    }
}

// ---------------------------------------------------------------- store

struct SideRow {
    digest: String,
    /// Encoded spec for spec kinds, UTF-8 Cedar for policies.
    payload: Vec<u8>,
}

/// Frozen draft bytes, when every ref pinned, and the draft-only dependencies.
struct FrozenDraft {
    side: Option<(String, SideRow)>,
    unreleased: Vec<UnreleasedDependency>,
}

fn payload_column(kind: SuggestKind) -> &'static str {
    match kind {
        SuggestKind::Policy => "cedar",
        _ => "spec_bytes",
    }
}

fn read_payload(
    row: &rusqlite::Row<'_>,
    kind: SuggestKind,
    idx: usize,
) -> rusqlite::Result<Vec<u8>> {
    match kind {
        SuggestKind::Policy => Ok(row.get::<_, String>(idx)?.into_bytes()),
        _ => row.get::<_, Vec<u8>>(idx),
    }
}

fn fetch_draft(
    connection: &rusqlite::Connection,
    kind: SuggestKind,
    urn: &str,
) -> Result<Option<SideRow>, OrchestratorError> {
    let sql = format!(
        "SELECT digest, {} FROM {} WHERE urn = ?1 AND channel = 'draft'",
        payload_column(kind),
        kind.table()
    );
    connection
        .query_row(&sql, params![urn], |row| {
            Ok(SideRow {
                digest: row.get(0)?,
                payload: read_payload(row, kind, 1)?,
            })
        })
        .optional()
        .map_err(OrchestratorError::Database)
}

fn fetch_latest(
    connection: &rusqlite::Connection,
    kind: SuggestKind,
    urn: &str,
) -> Result<Option<(ReleaseTriple, SideRow)>, OrchestratorError> {
    let sql = format!(
        "SELECT major, minor, patch, digest, {} FROM {}
         WHERE urn = ?1 AND channel = 'released'
         ORDER BY major DESC, minor DESC, patch DESC LIMIT 1",
        payload_column(kind),
        kind.table()
    );
    connection
        .query_row(&sql, params![urn], |row| {
            Ok((
                (row.get(0)?, row.get(1)?, row.get(2)?),
                SideRow {
                    digest: row.get(3)?,
                    payload: read_payload(row, kind, 4)?,
                },
            ))
        })
        .optional()
        .map_err(OrchestratorError::Database)
}

fn decode<M: Message + Default>(bytes: &[u8]) -> Result<M, OrchestratorError> {
    M::decode(bytes).map_err(|e| OrchestratorError::Core(e.to_string()))
}

fn classify_rows(
    kind: SuggestKind,
    old: &SideRow,
    new: &SideRow,
) -> Result<Classification, OrchestratorError> {
    Ok(match kind {
        SuggestKind::Agent => classify_agent(&decode(&old.payload)?, &decode(&new.payload)?),
        SuggestKind::Playbook => classify_playbook(&decode(&old.payload)?, &decode(&new.payload)?),
        SuggestKind::Skill | SuggestKind::SkillPackage => {
            classify_skill(&decode(&old.payload)?, &decode(&new.payload)?)
        }
        SuggestKind::Policy => classify_policy(
            &String::from_utf8_lossy(&old.payload),
            &String::from_utf8_lossy(&new.payload),
        ),
    })
}

/// The draft manifest's own version, used as the first release of a package.
fn manifest_triple(draft: &SideRow) -> Result<Option<ReleaseTriple>, OrchestratorError> {
    let manifest: SkillManifest = decode(&draft.payload)?;
    Ok(manifest.version.map(|v| (v.major, v.minor, v.patch)))
}

impl SqliteWorkflowStore {
    /// The draft as a release would store it, plus the draft-only dependencies
    /// a release would refuse. Agents and playbooks only; other kinds carry no
    /// refs and compare as stored.
    ///
    /// A draft with a dependency that is not released cannot be frozen. The
    /// suggestion still answers, comparing the draft as authored, and lists
    /// those dependencies in release order. The release itself fails with the
    /// same 409 text as before.
    fn frozen_draft_side(
        &self,
        kind: SuggestKind,
        urn: &str,
    ) -> Result<FrozenDraft, OrchestratorError> {
        if !matches!(kind, SuggestKind::Agent | SuggestKind::Playbook) {
            return Ok(FrozenDraft {
                side: None,
                unreleased: Vec::new(),
            });
        }
        let (draft, owner) = {
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            let owner: Option<String> = connection
                .query_row(
                    &format!(
                        "SELECT owner_user_id FROM {} WHERE urn = ?1 AND channel = 'draft'",
                        kind.table()
                    ),
                    params![urn],
                    |row| row.get(0),
                )
                .optional()?;
            (
                fetch_draft(&connection, kind, urn)?
                    .ok_or_else(|| release_no_draft_error(kind.noun(), urn))?,
                owner.unwrap_or_default(),
            )
        };
        let subject = match kind {
            SuggestKind::Agent => crate::catalog_freeze::FreezeSubject::Agent,
            _ => crate::catalog_freeze::FreezeSubject::Playbook,
        };
        let frozen = match kind {
            SuggestKind::Agent => {
                crate::catalog_freeze::freeze_agent(self, &draft.payload, &draft.digest, &owner)
                    .map(|f| (f.bytes, f.digest))
            }
            _ => {
                crate::catalog_freeze::freeze_playbook(self, &draft.payload, &draft.digest, &owner)
                    .map(|f| (f.bytes, f.digest))
            }
        };
        let side = match frozen {
            Ok((payload, digest)) => Some((draft.digest, SideRow { digest, payload })),
            Err(OrchestratorError::DependenciesNotReleased(_)) => None,
            Err(err) => return Err(err),
        };
        let unreleased =
            crate::catalog_freeze::unreleased_dependencies(self, subject, &draft.payload, &owner)?;
        Ok(FrozenDraft { side, unreleased })
    }

    /// Suggest the next release of the draft of `urn`. Callers check release
    /// authorization first. With no draft this is a 404.
    ///
    /// For agents and playbooks the draft is frozen first and compared with the
    /// latest release, which is stored frozen. "Unchanged" and the reasons then
    /// speak about what a release would actually write.
    pub fn release_suggestion(
        &self,
        kind: SuggestKind,
        urn: &str,
    ) -> Result<ReleaseSuggestion, OrchestratorError> {
        let frozen = self.frozen_draft_side(kind, urn)?;
        let unreleased = frozen.unreleased;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut draft = fetch_draft(&connection, kind, urn)?
            .ok_or_else(|| release_no_draft_error(kind.noun(), urn))?;
        // Use the frozen side only if the draft is the one that was frozen.
        if let Some((draft_digest, side)) = frozen.side {
            if draft_digest == draft.digest {
                draft = side;
            }
        }
        let manifest_version = match kind {
            SuggestKind::SkillPackage => manifest_triple(&draft)?,
            _ => None,
        };
        let Some((latest, released)) = fetch_latest(&connection, kind, urn)? else {
            return Ok(with_unreleased(
                build_suggestion(Compared {
                    latest: None,
                    latest_digest: String::new(),
                    draft_digest: draft.digest,
                    manifest_version,
                    classification: None,
                }),
                unreleased,
            ));
        };
        let classification = if released.digest == draft.digest {
            None
        } else {
            Some(classify_rows(kind, &released, &draft)?)
        };
        Ok(with_unreleased(
            build_suggestion(Compared {
                latest: Some(latest),
                latest_digest: released.digest,
                draft_digest: draft.digest,
                manifest_version,
                classification,
            }),
            unreleased,
        ))
    }
}

fn with_unreleased(
    mut suggestion: ReleaseSuggestion,
    unreleased: Vec<UnreleasedDependency>,
) -> ReleaseSuggestion {
    suggestion.unreleased_dependencies = unreleased;
    suggestion
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::{
        agent_spec::InstructionSource, AgentPermissions, CapabilityPolicyRef, Digest, HostDefault,
        InternetPermissions, PermissionChangeKind, PermissionDecision, PermissionRule, ResourceId,
        StepOutcome, ToolDefault, ToolPermissions, ToolSpec,
    };

    fn rref(urn: &str, patch: i32) -> ResourceRef {
        ResourceRef {
            resource_id: Some(ResourceId { urn: urn.into() }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch,
                prerelease: String::new(),
                build: String::new(),
            }),
            digest: Some(Digest::default()),
            locators: Vec::new(),
        }
    }

    fn skill_urn(name: &str) -> String {
        format!("urn:vam:skill:org.example/acme/{name}")
    }

    fn playbook_urn(name: &str) -> String {
        format!("urn:vam:playbook:org.example/acme/{name}")
    }

    fn instructions(text: &str) -> Option<InstructionSource> {
        Some(InstructionSource::SystemInstructions(text.into()))
    }

    fn base_agent() -> AgentSpec {
        AgentSpec {
            instruction_source: instructions("be kind"),
            skill_refs: vec![rref(&skill_urn("search"), 0)],
            allowed_playbook_refs: vec![rref(&playbook_urn("triage"), 0)],
            default_playbook_ref: Some(rref(&playbook_urn("triage"), 0)),
            capability_policy: Some(CapabilityPolicyRef {
                policy_ref: Some(rref("urn:vam:policy:org.example/acme/base", 0)),
                security_profile_name: String::new(),
            }),
            extends: Some(rref("urn:vam:agent:org.example/acme/parent", 0)),
            ..Default::default()
        }
    }

    struct Row<T> {
        name: &'static str,
        edit: fn(&mut T),
        level: Level,
        reasons: &'static [&'static str],
    }

    #[test]
    fn agent_rules() {
        let rows: Vec<Row<AgentSpec>> = vec![
            Row {
                name: "skill removed",
                edit: |a| a.skill_refs.clear(),
                level: Level::Major,
                reasons: &["removed skill search"],
            },
            Row {
                name: "playbook removed from allowedPlaybookRefs",
                edit: |a| a.allowed_playbook_refs.clear(),
                level: Level::Major,
                reasons: &["removed playbook triage"],
            },
            Row {
                name: "playbook removed from defaultPlaybookRef",
                edit: |a| a.default_playbook_ref = None,
                level: Level::Major,
                reasons: &["removed default playbook triage"],
            },
            Row {
                name: "capability policy changed to a different URN",
                edit: |a| {
                    a.capability_policy = Some(CapabilityPolicyRef {
                        policy_ref: Some(rref("urn:vam:policy:org.example/acme/other", 0)),
                        security_profile_name: String::new(),
                    })
                },
                level: Level::Major,
                reasons: &["changed capability policy"],
            },
            Row {
                name: "extends changed",
                edit: |a| a.extends = Some(rref("urn:vam:agent:org.example/acme/other", 0)),
                level: Level::Major,
                reasons: &["changed extends"],
            },
            Row {
                name: "extends pin changed (ambiguous: higher bump)",
                edit: |a| a.extends = Some(rref("urn:vam:agent:org.example/acme/parent", 1)),
                level: Level::Major,
                reasons: &["changed extends"],
            },
            Row {
                name: "skill added",
                edit: |a| a.skill_refs.push(rref(&skill_urn("jira"), 0)),
                level: Level::Minor,
                reasons: &["added skill jira"],
            },
            Row {
                name: "playbook added",
                edit: |a| {
                    a.allowed_playbook_refs
                        .push(rref(&playbook_urn("release"), 0))
                },
                level: Level::Minor,
                reasons: &["added playbook release"],
            },
            Row {
                name: "instructions only",
                edit: |a| a.instruction_source = instructions("v2"),
                level: Level::Patch,
                reasons: &["edited instructions"],
            },
            Row {
                name: "display name, description and role",
                edit: |a| {
                    a.display_name = "New".into();
                    a.description = "New".into();
                    a.role = "New".into();
                },
                level: Level::Patch,
                reasons: &["edited display name", "edited description", "edited role"],
            },
            Row {
                name: "skill pin bumped within the same URN",
                edit: |a| a.skill_refs = vec![rref(&skill_urn("search"), 1)],
                level: Level::Patch,
                reasons: &["updated skill search to v1.0.1"],
            },
            Row {
                name: "skill removed plus instructions edited",
                edit: |a| {
                    a.skill_refs.clear();
                    a.instruction_source = instructions("v2");
                },
                level: Level::Major,
                reasons: &["removed skill search", "edited instructions"],
            },
            Row {
                name: "skill swapped: highest wins, both listed",
                edit: |a| a.skill_refs = vec![rref(&skill_urn("jira"), 0)],
                level: Level::Major,
                reasons: &["removed skill search", "added skill jira"],
            },
        ];
        for row in rows {
            let mut new = base_agent();
            (row.edit)(&mut new);
            let got = classify_agent(&base_agent(), &new);
            assert_eq!(got.level, row.level, "{}", row.name);
            assert_eq!(got.reasons, row.reasons, "{}", row.name);
            assert!(
                got.permission_changes.is_empty(),
                "{} touched permissions",
                row.name
            );
        }
    }

    #[test]
    fn agent_allow_any_playbook_flips() {
        let mut off = base_agent();
        off.allow_any_playbook = false;
        let mut on = base_agent();
        on.allow_any_playbook = true;
        let down = classify_agent(&on, &off);
        assert_eq!(down.level, Level::Major);
        assert_eq!(down.reasons, ["allowAnyPlaybook turned off"]);
        let up = classify_agent(&off, &on);
        assert_eq!(up.level, Level::Minor);
        assert_eq!(up.reasons, ["allowAnyPlaybook turned on"]);
    }

    fn plain_agent() -> AgentSpec {
        AgentSpec {
            instruction_source: instructions("be kind"),
            ..Default::default()
        }
    }

    fn tool_rule(target: &str, decision: PermissionDecision) -> PermissionRule {
        PermissionRule {
            target: target.to_string(),
            decision: decision as i32,
        }
    }

    fn tools(default: ToolDefault, rules: &[(&str, PermissionDecision)]) -> AgentPermissions {
        AgentPermissions {
            tools: Some(ToolPermissions {
                default: default as i32,
                rules: rules
                    .iter()
                    .map(|(target, decision)| tool_rule(target, *decision))
                    .collect(),
            }),
            internet: None,
            advanced: Vec::new(),
            web: Default::default(),
        }
    }

    fn hosts(rules: &[(&str, PermissionDecision)]) -> AgentPermissions {
        AgentPermissions {
            tools: None,
            internet: Some(InternetPermissions {
                default: HostDefault::Allow as i32,
                rules: rules
                    .iter()
                    .map(|(target, decision)| tool_rule(target, *decision))
                    .collect(),
            }),
            advanced: Vec::new(),
            web: Default::default(),
        }
    }

    fn agent_with(perms: AgentPermissions) -> AgentSpec {
        let mut agent = plain_agent();
        agent.permissions = Some(perms);
        agent
    }

    struct ToolRow {
        name: &'static str,
        old_default: ToolDefault,
        new_default: ToolDefault,
        old_rules: Vec<(&'static str, PermissionDecision)>,
        new_rules: Vec<(&'static str, PermissionDecision)>,
        level: Level,
        target: &'static str,
        before: &'static str,
        after: &'static str,
        kind: PermissionChangeKind,
        sentence: &'static str,
    }

    #[test]
    fn permission_decision_changes_set_the_bump_and_the_sentence() {
        let mcp = "mcp:mcp.atlassian.com/create_issue";
        let jira = "skill:jira/create_issue";
        let rows = vec![
            ToolRow {
                name: "J3 allow added over a released ask",
                old_default: ToolDefault::Ask,
                new_default: ToolDefault::Ask,
                old_rules: vec![],
                new_rules: vec![(mcp, PermissionDecision::Allow)],
                level: Level::Minor,
                target: mcp,
                before: "ask",
                after: "allow",
                kind: PermissionChangeKind::Loosened,
                sentence:
                    "Now allowed without asking: `mcp:mcp.atlassian.com/create_issue` (was: ask)",
            },
            ToolRow {
                name: "J4 allow to ask",
                old_default: ToolDefault::BySideEffect,
                new_default: ToolDefault::BySideEffect,
                old_rules: vec![(jira, PermissionDecision::Allow)],
                new_rules: vec![(jira, PermissionDecision::Ask)],
                level: Level::Minor,
                target: jira,
                before: "allow",
                after: "ask",
                kind: PermissionChangeKind::Tightened,
                sentence: "Now asks: `skill:jira/create_issue` (was: allow)",
            },
            ToolRow {
                name: "J4 ask to deny",
                old_default: ToolDefault::BySideEffect,
                new_default: ToolDefault::BySideEffect,
                old_rules: vec![(jira, PermissionDecision::Ask)],
                new_rules: vec![(jira, PermissionDecision::Deny)],
                level: Level::Major,
                target: jira,
                before: "ask",
                after: "deny",
                kind: PermissionChangeKind::Tightened,
                sentence: "Now denied: `skill:jira/create_issue`",
            },
            ToolRow {
                name: "J4 allow to deny",
                old_default: ToolDefault::BySideEffect,
                new_default: ToolDefault::BySideEffect,
                old_rules: vec![(jira, PermissionDecision::Allow)],
                new_rules: vec![(jira, PermissionDecision::Deny)],
                level: Level::Major,
                target: jira,
                before: "allow",
                after: "deny",
                kind: PermissionChangeKind::Tightened,
                sentence: "Now denied: `skill:jira/create_issue`",
            },
            ToolRow {
                name: "J4 ask to allow is minor, not patch",
                old_default: ToolDefault::BySideEffect,
                new_default: ToolDefault::BySideEffect,
                old_rules: vec![(jira, PermissionDecision::Ask)],
                new_rules: vec![(jira, PermissionDecision::Allow)],
                level: Level::Minor,
                target: jira,
                before: "ask",
                after: "allow",
                kind: PermissionChangeKind::Loosened,
                sentence: "Now allowed without asking: `skill:jira/create_issue` (was: ask)",
            },
            ToolRow {
                name: "bySideEffect to ask",
                old_default: ToolDefault::BySideEffect,
                new_default: ToolDefault::Ask,
                old_rules: vec![],
                new_rules: vec![],
                level: Level::Minor,
                target: "tools.default",
                before: "bySideEffect",
                after: "ask",
                kind: PermissionChangeKind::Tightened,
                sentence: "Now asks: `tools.default` (was: bySideEffect)",
            },
            ToolRow {
                name: "removed rule falls back to the source wildcard",
                old_default: ToolDefault::BySideEffect,
                new_default: ToolDefault::BySideEffect,
                old_rules: vec![
                    ("skill:jira/*", PermissionDecision::Ask),
                    (jira, PermissionDecision::Allow),
                ],
                new_rules: vec![("skill:jira/*", PermissionDecision::Ask)],
                level: Level::Minor,
                target: jira,
                before: "allow",
                after: "ask",
                kind: PermissionChangeKind::Tightened,
                sentence: "Now asks: `skill:jira/create_issue` (was: allow)",
            },
            ToolRow {
                name: "removed rule that matches the default adds nothing",
                old_default: ToolDefault::Allow,
                new_default: ToolDefault::Allow,
                old_rules: vec![(jira, PermissionDecision::Allow)],
                new_rules: vec![],
                level: Level::Patch,
                target: "",
                before: "",
                after: "",
                kind: PermissionChangeKind::Unspecified,
                sentence: "",
            },
        ];
        for row in rows {
            let old = agent_with(tools(row.old_default, &row.old_rules));
            let new = agent_with(tools(row.new_default, &row.new_rules));
            let got = classify_agent(&old, &new);
            assert_eq!(got.level, row.level, "{}", row.name);
            if row.sentence.is_empty() {
                assert!(
                    got.permission_changes.is_empty(),
                    "{}: {:?}",
                    row.name,
                    got.permission_changes
                );
                assert_eq!(got.reasons, ["other changes"], "{}", row.name);
                continue;
            }
            assert_eq!(got.reasons, [row.sentence], "{}", row.name);
            assert_eq!(got.permission_changes.len(), 1, "{}", row.name);
            let change = &got.permission_changes[0];
            assert_eq!(change.target, row.target, "{}", row.name);
            assert_eq!(change.before, row.before, "{}", row.name);
            assert_eq!(change.after, row.after, "{}", row.name);
            assert_eq!(change.change, row.kind as i32, "{}", row.name);
            assert_eq!(change.sentence, row.sentence, "{}", row.name);
            assert_ne!(row.level, Level::Patch, "{}", row.name);
        }
    }

    #[test]
    fn j3_suggestion_bumps_minor_and_carries_the_permission_change() {
        let mcp = "mcp:mcp.atlassian.com/create_issue";
        let old = agent_with(tools(ToolDefault::Ask, &[]));
        let new = agent_with(tools(ToolDefault::Ask, &[(mcp, PermissionDecision::Allow)]));
        let classification = classify_agent(&old, &new);
        let suggestion = build_suggestion(Compared {
            latest: Some((1, 0, 0)),
            latest_digest: "released".into(),
            draft_digest: "draft".into(),
            manifest_version: None,
            classification: Some(classification),
        });
        assert_eq!(suggestion.bump, ReleaseBump::Minor as i32);
        assert_eq!(triple(&suggestion.suggested), (1, 1, 0));
        assert_eq!(suggestion.permission_changes.len(), 1);
        assert_eq!(
            suggestion.permission_changes[0].sentence,
            "Now allowed without asking: `mcp:mcp.atlassian.com/create_issue` (was: ask)"
        );
    }

    #[test]
    fn unchanged_permissions_leave_the_instruction_suggestion_untouched() {
        let perms = tools(
            ToolDefault::Ask,
            &[("skill:jira/create_issue", PermissionDecision::Ask)],
        );
        let old = agent_with(perms.clone());
        let mut new = old.clone();
        new.instruction_source = instructions("v2");
        let got = classify_agent(&old, &new);
        assert_eq!(got.level, Level::Patch);
        assert_eq!(got.reasons, ["edited instructions"]);
        assert!(got.permission_changes.is_empty());
    }

    fn with_web(mut perms: AgentPermissions, enabled: bool) -> AgentPermissions {
        perms.web = Some(vam_spec::v1::WebPermissions { enabled });
        perms
    }

    #[test]
    fn turning_web_on_is_minor_and_off_is_major() {
        let base = tools(ToolDefault::BySideEffect, &[]);
        let off = agent_with(base.clone());
        let on = agent_with(with_web(base.clone(), true));

        let got = classify_agent(&off, &on);
        assert_eq!(got.level, Level::Minor);
        assert_eq!(got.permission_changes.len(), 1);
        let change = &got.permission_changes[0];
        assert_eq!(
            (
                change.target.as_str(),
                change.before.as_str(),
                change.after.as_str()
            ),
            ("web", "off", "on")
        );
        assert_eq!(change.change, PermissionChangeKind::Loosened as i32);

        let got = classify_agent(&on, &off);
        assert_eq!(got.level, Level::Major);
        assert_eq!(got.permission_changes[0].after, "off");
        assert_eq!(
            got.permission_changes[0].change,
            PermissionChangeKind::Tightened as i32
        );

        // `web: {enabled: false}` and an absent `web` are the same.
        let explicit_off = agent_with(with_web(base, false));
        let got = classify_agent(&off, &explicit_off);
        assert!(got.permission_changes.is_empty(), "{got:?}");
    }

    #[test]
    fn host_deny_is_major_and_names_the_pattern() {
        let old = agent_with(hosts(&[]));
        let new = agent_with(hosts(&[("*.pastebin.com", PermissionDecision::Deny)]));
        let got = classify_agent(&old, &new);
        assert_eq!(got.level, Level::Major);
        assert_eq!(got.permission_changes.len(), 1);
        let change = &got.permission_changes[0];
        assert_eq!(change.target, "*.pastebin.com");
        assert_eq!(change.before, "allow");
        assert_eq!(change.after, "deny");
        assert_eq!(change.change, PermissionChangeKind::Tightened as i32);
        assert_eq!(change.sentence, "Now denied: `*.pastebin.com`");
    }

    #[test]
    fn removed_host_rule_falls_back_to_the_broader_wildcard() {
        let old = agent_with(hosts(&[
            ("*.com", PermissionDecision::Deny),
            ("*.pastebin.com", PermissionDecision::Allow),
        ]));
        let new = agent_with(hosts(&[("*.com", PermissionDecision::Deny)]));
        let got = classify_agent(&old, &new);
        assert_eq!(got.level, Level::Major);
        assert_eq!(got.permission_changes.len(), 1);
        let change = &got.permission_changes[0];
        assert_eq!(change.target, "*.pastebin.com");
        assert_eq!(change.before, "allow");
        assert_eq!(change.after, "deny");
        assert_eq!(change.sentence, "Now denied: `*.pastebin.com`");
    }

    #[test]
    fn loosening_away_from_deny_is_minor() {
        let old = agent_with(hosts(&[("*.pastebin.com", PermissionDecision::Deny)]));
        let new = agent_with(hosts(&[]));
        let got = classify_agent(&old, &new);
        assert_eq!(got.level, Level::Minor);
        assert_eq!(
            got.permission_changes[0].sentence,
            "Now allowed without asking: `*.pastebin.com` (was: deny)"
        );
        assert_eq!(
            got.permission_changes[0].change,
            PermissionChangeKind::Loosened as i32
        );
    }

    #[test]
    fn advanced_ref_added_or_removed_is_minor() {
        let policy = "urn:vam:policy:org.example/acme/extra";
        let mut added = plain_agent();
        added.permissions = Some(AgentPermissions {
            advanced: vec![rref(policy, 0)],
            ..Default::default()
        });
        let got = classify_agent(&plain_agent(), &added);
        assert_eq!(got.level, Level::Minor);
        assert_eq!(got.permission_changes.len(), 1);
        assert_eq!(got.permission_changes[0].target, policy);
        assert_eq!(
            got.permission_changes[0].change,
            PermissionChangeKind::Added as i32
        );
        assert_eq!(
            got.permission_changes[0].sentence,
            format!("Added advanced policy `{policy}`")
        );

        let got = classify_agent(&added, &plain_agent());
        assert_eq!(got.level, Level::Minor);
        assert_eq!(
            got.permission_changes[0].change,
            PermissionChangeKind::Removed as i32
        );
        assert_eq!(
            got.permission_changes[0].sentence,
            format!("Removed advanced policy `{policy}`")
        );

        let mut pinned = added.clone();
        pinned.permissions.as_mut().unwrap().advanced = vec![rref(policy, 1)];
        let got = classify_agent(&added, &pinned);
        assert_eq!(got.level, Level::Patch);
        assert!(got.permission_changes.is_empty());
        assert_eq!(got.reasons, ["updated policy extra to v1.0.1"]);
    }

    #[test]
    fn legacy_capability_policy_becomes_one_minor_permissions_entry() {
        let old = base_agent();
        let mut new = base_agent();
        new.capability_policy = None;
        new.permissions = Some(tools(
            ToolDefault::Ask,
            &[(
                "mcp:mcp.atlassian.com/create_issue",
                PermissionDecision::Allow,
            )],
        ));
        let got = classify_agent(&old, &new);
        assert_eq!(got.level, Level::Minor);
        assert_eq!(got.permission_changes.len(), 1);
        assert_eq!(got.permission_changes[0].target, "permissions");
        assert_eq!(got.permission_changes[0].before, "cedar");
        assert_eq!(got.permission_changes[0].after, "rules");
        assert_eq!(
            got.permission_changes[0].change,
            PermissionChangeKind::Added as i32
        );
        assert_eq!(
            got.permission_changes[0].sentence,
            "Permissions moved from a Cedar policy to rules; review the effective view"
        );
        assert_eq!(
            got.reasons,
            ["Permissions moved from a Cedar policy to rules; review the effective view"]
        );
    }

    #[test]
    fn permission_bump_combines_with_existing_rules_by_taking_the_largest() {
        let jira = "skill:jira/create_issue";
        let mut old = plain_agent();
        old.skill_refs = vec![rref(&skill_urn("search"), 0)];
        old.permissions = Some(tools(
            ToolDefault::BySideEffect,
            &[(jira, PermissionDecision::Allow)],
        ));
        let mut new = old.clone();
        new.skill_refs.clear();
        new.permissions = Some(tools(
            ToolDefault::BySideEffect,
            &[(jira, PermissionDecision::Ask)],
        ));
        let got = classify_agent(&old, &new);
        assert_eq!(got.level, Level::Major);
        assert_eq!(
            got.reasons,
            [
                "removed skill search",
                "Now asks: `skill:jira/create_issue` (was: allow)",
            ]
        );

        let mut released_ask = old.clone();
        released_ask.permissions = Some(tools(
            ToolDefault::BySideEffect,
            &[(jira, PermissionDecision::Ask)],
        ));
        let mut draft = old.clone();
        draft.instruction_source = instructions("v2");
        let got = classify_agent(&released_ask, &draft);
        assert_eq!(got.level, Level::Minor);
        assert_eq!(
            got.reasons,
            [
                "edited instructions",
                "Now allowed without asking: `skill:jira/create_issue` (was: ask)",
            ]
        );
    }

    fn step(id: &str, options: &[&str]) -> PlaybookStep {
        PlaybookStep {
            id: id.into(),
            instruction: format!("do {id}"),
            options: options
                .iter()
                .map(|label| StepOutcome {
                    label: (*label).into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn schema(required: &[&str], properties: &[&str]) -> prost_types::Struct {
        use prost_types::value::Kind;
        let value = |kind| prost_types::Value { kind: Some(kind) };
        let mut fields = BTreeMap::new();
        fields.insert(
            "required".to_string(),
            value(Kind::ListValue(prost_types::ListValue {
                values: required
                    .iter()
                    .map(|r| value(Kind::StringValue((*r).into())))
                    .collect(),
            })),
        );
        fields.insert(
            "properties".to_string(),
            value(Kind::StructValue(prost_types::Struct {
                fields: properties
                    .iter()
                    .map(|p| ((*p).to_string(), value(Kind::NullValue(0))))
                    .collect(),
            })),
        );
        prost_types::Struct { fields }
    }

    fn base_playbook() -> PlaybookSpec {
        PlaybookSpec {
            activation: Some(vam_spec::v1::PlaybookActivation::asking(
                "Use when the request matches this test playbook.",
            )),
            steps: vec![step("a", &[]), step("approve", &["yes", "no"])],
            input_schema: Some(schema(&["ticket"], &["ticket", "note"])),
            ..Default::default()
        }
    }

    #[test]
    fn playbook_rules() {
        let rows: Vec<Row<PlaybookSpec>> = vec![
            Row {
                name: "step removed",
                edit: |p| p.steps.retain(|s| s.id != "a"),
                level: Level::Major,
                reasons: &["removed step a"],
            },
            Row {
                name: "required input added",
                edit: |p| {
                    p.input_schema = Some(schema(&["ticket", "env"], &["ticket", "note", "env"]))
                },
                level: Level::Major,
                reasons: &["added required input env"],
            },
            Row {
                name: "optional input turned required (ambiguous: higher bump)",
                edit: |p| p.input_schema = Some(schema(&["ticket", "note"], &["ticket", "note"])),
                level: Level::Major,
                reasons: &["added required input note"],
            },
            Row {
                name: "gate option removed",
                edit: |p| p.steps[1] = step("approve", &["yes"]),
                level: Level::Major,
                reasons: &["removed option no from gate approve"],
            },
            Row {
                name: "step added",
                edit: |p| p.steps.push(step("c", &[])),
                level: Level::Minor,
                reasons: &["added step c"],
            },
            Row {
                name: "optional input added",
                edit: |p| p.input_schema = Some(schema(&["ticket"], &["ticket", "note", "tag"])),
                level: Level::Minor,
                reasons: &["added optional input tag"],
            },
            Row {
                name: "step instruction text",
                edit: |p| p.steps[0].instruction = "changed".into(),
                level: Level::Patch,
                reasons: &["edited step instructions"],
            },
            Row {
                name: "descriptions",
                edit: |p| {
                    p.display_name = "n".into();
                    p.description = "d".into();
                },
                level: Level::Patch,
                reasons: &["edited display name", "edited description"],
            },
            Row {
                name: "step removed plus step added",
                edit: |p| {
                    p.steps.retain(|s| s.id != "a");
                    p.steps.push(step("c", &[]));
                },
                level: Level::Major,
                reasons: &["removed step a", "added step c"],
            },
        ];
        for row in rows {
            let mut new = base_playbook();
            (row.edit)(&mut new);
            let got = classify_playbook(&base_playbook(), &new);
            assert_eq!(got.level, row.level, "{}", row.name);
            assert_eq!(got.reasons, row.reasons, "{}", row.name);
        }
    }

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            ..Default::default()
        }
    }

    fn base_skill() -> SkillManifest {
        SkillManifest {
            capabilities: vec!["tool:jira.search".into(), "egress:jira.example.com".into()],
            tools: vec![tool("search"), tool("create")],
            ..Default::default()
        }
    }

    #[test]
    fn skill_rules() {
        let rows: Vec<Row<SkillManifest>> = vec![
            Row {
                name: "capability removed",
                edit: |s| {
                    s.capabilities.pop();
                },
                level: Level::Major,
                reasons: &["removed capability egress:jira.example.com"],
            },
            Row {
                name: "tool removed",
                edit: |s| {
                    s.tools.pop();
                },
                level: Level::Major,
                reasons: &["removed tool create"],
            },
            Row {
                name: "capability added",
                edit: |s| s.capabilities.push("secret:JIRA_TOKEN".into()),
                level: Level::Minor,
                reasons: &["added capability secret:JIRA_TOKEN"],
            },
            Row {
                name: "tool added",
                edit: |s| s.tools.push(tool("close")),
                level: Level::Minor,
                reasons: &["added tool close"],
            },
            Row {
                name: "description only",
                edit: |s| s.description = "new".into(),
                level: Level::Patch,
                reasons: &["edited description"],
            },
            Row {
                name: "tool removed plus capability added",
                edit: |s| {
                    s.tools.pop();
                    s.capabilities.push("fs:workspace:write".into());
                },
                level: Level::Major,
                reasons: &["added capability fs:workspace:write", "removed tool create"],
            },
        ];
        for row in rows {
            let mut new = base_skill();
            (row.edit)(&mut new);
            let got = classify_skill(&base_skill(), &new);
            assert_eq!(got.level, row.level, "{}", row.name);
            assert_eq!(got.reasons, row.reasons, "{}", row.name);
        }
    }

    #[test]
    fn policy_any_cedar_change_is_minor_and_says_so() {
        let got = classify_policy(
            "permit(principal, action, resource);",
            "forbid(principal, action, resource);",
        );
        assert_eq!(got.level, Level::Minor);
        assert_eq!(got.reasons.len(), 1);
        assert!(got.reasons[0].contains("Cedar cannot be classified reliably"));
    }

    fn compared(
        latest: Option<ReleaseTriple>,
        latest_digest: &str,
        draft_digest: &str,
    ) -> Compared {
        Compared {
            latest,
            latest_digest: latest_digest.into(),
            draft_digest: draft_digest.into(),
            manifest_version: None,
            classification: None,
        }
    }

    fn triple(v: &Option<Version>) -> ReleaseTriple {
        let v = v.as_ref().expect("version");
        (v.major, v.minor, v.patch)
    }

    #[test]
    fn no_release_is_initial_one_zero_zero() {
        let s = build_suggestion(compared(None, "", "d1"));
        assert_eq!(s.bump, ReleaseBump::Initial as i32);
        assert_eq!(triple(&s.suggested), (1, 0, 0));
        assert!(s.latest.is_none());
        assert!(!s.unchanged);
    }

    #[test]
    fn skill_package_initial_uses_manifest_version_above_zero() {
        let mut c = compared(None, "", "d1");
        c.manifest_version = Some((0, 3, 2));
        assert_eq!(triple(&build_suggestion(c).suggested), (0, 3, 2));
        let mut zero = compared(None, "", "d1");
        zero.manifest_version = Some((0, 0, 0));
        assert_eq!(triple(&build_suggestion(zero).suggested), (1, 0, 0));
    }

    #[test]
    fn unchanged_draft_is_none_with_patch_plus_one() {
        let s = build_suggestion(compared(Some((1, 2, 3)), "same", "same"));
        assert_eq!(s.bump, ReleaseBump::None as i32);
        assert!(s.unchanged);
        assert_eq!(triple(&s.latest), (1, 2, 3));
        assert_eq!(triple(&s.suggested), (1, 2, 4));
        assert!(s.reasons.is_empty());
    }

    #[test]
    fn bump_levels_map_to_next_version() {
        for (level, bump, want) in [
            (Level::Patch, ReleaseBump::Patch, (1, 2, 4)),
            (Level::Minor, ReleaseBump::Minor, (1, 3, 0)),
            (Level::Major, ReleaseBump::Major, (2, 0, 0)),
        ] {
            let mut c = compared(Some((1, 2, 3)), "old", "new");
            c.classification = Some(Classification {
                level,
                reasons: vec!["r".into()],
                permission_changes: Vec::new(),
            });
            let s = build_suggestion(c);
            assert_eq!(s.bump, bump as i32);
            assert_eq!(triple(&s.suggested), want);
            assert_eq!(s.reasons, ["r"]);
        }
    }

    #[test]
    fn changed_digest_with_no_matched_rule_is_patch_other_changes() {
        let m = Matches::default().finish();
        assert_eq!(m.level, Level::Patch);
        assert_eq!(m.reasons, ["other changes"]);
    }

    #[test]
    fn urn_name_is_last_segment() {
        assert_eq!(urn_name("urn:vam:skill:org.example/acme/jira"), "jira");
    }
}
