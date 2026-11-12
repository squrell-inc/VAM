//! Inheritance merge for `AgentSpec.extends`.
//!
//! `extends` is inheritance of a pinned agent definition. It is not a call.
//! The child does not invoke the parent, and this module does not delegate.
//!
//! Merge rules are locked (docs: authoring/agent-spec). The caller resolves the chain
//! and passes it root-first. This function does not fetch from the catalog.
//!
//! # What merges
//!
//! Identity stays on the child. Models, context, reasoning, resources, policy
//! refs, and playbook refs are not inherited. The result is inline
//! instructions, the effective skill refs, and the effective capability tokens.
//!
//! # Verbs
//!
//! | Axis | Verb | Effect |
//! |---|---|---|
//! | instructions | `append` | parent text, a blank line, then the child text |
//! | instructions | `replace` | the child text only |
//! | skills | `add` | union the ref into the effective set |
//! | skills | `remove` | drop that skill URN |
//! | capabilities | `grant` | add the token |
//! | capabilities | `revoke` | remove the token |
//!
//! An absent op inherits that axis.
//!
//! # Conflicts
//!
//! * Chains longer than [`MAX_EXTENDS_DEPTH`] hops fail with
//!   [`AgentMergeError::DepthExceeded`].
//! * A repeated agent URN fails with [`AgentMergeError::Cycle`]. A spec that
//!   extends its own URN fails validation (`extends_cycle`) before merge.
//! * A root that itself extends fails with [`AgentMergeError::Unresolved`].
//! * A child whose parent URN is not the previous agent fails with
//!   [`AgentMergeError::ParentMismatch`].
//! * Skill identity is the URN. The pin is version plus digest. A missing
//!   version or digest is a different pin from a present one. `remove` runs
//!   before `add` and matches the URN only. Adding a URN that is already
//!   present at a different pin fails with
//!   [`AgentMergeError::SkillVersionConflict`]. That is the diamond: A
//!   extends B, and both reference skill S at different versions, including
//!   when the older pin was introduced above the immediate parent.
//!   `remove` plus `add` of the same URN replaces the pin.
//! * After the skill edit, required capabilities are the union of the
//!   declarations the caller supplied for the skills that remain. A missing
//!   declaration fails with [`AgentMergeError::SkillCapabilitiesMissing`].
//! * Revoke of a token a remaining skill requires fails with
//!   [`AgentMergeError::CapabilityRevokeRequired`], including a skill the
//!   child itself added. Remove the skill, or do not revoke the token.
//!   Revoke of a token nothing requires is a no-op.
//! * The same token in `grant` and `revoke` fails validation
//!   (`capability_grant_revoke_conflict`).
//! * Append and inherit need inline parent text. An `instruction_artifact`
//!   parent fails those ops with
//!   [`AgentMergeError::InstructionArtifactOpaque`]. Replace still succeeds.

use crate::v1::{
    agent_spec::InstructionSource, instruction_overlay::Change, AgentOverlay, AgentSpec,
    ResourceRef,
};
use crate::validation::{Validate, ValidationError};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use thiserror::Error;

/// Maximum number of `extends` hops. A chain of 9 agents is 8 hops and is
/// allowed. One more hop is not.
pub const MAX_EXTENDS_DEPTH: usize = 8;

/// Instructions, skills, and capabilities after the overlay chain is applied.
///
/// `system_instructions` is `None` only when the root agent's instructions are
/// an external artifact and no child replaced them.
#[derive(Debug, Clone, PartialEq)]
pub struct MergedAgent {
    pub system_instructions: Option<String>,
    pub skills: Vec<ResourceRef>,
    pub capabilities: Vec<String>,
}

/// Why an `extends` chain could not be merged.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum AgentMergeError {
    #[error("extends chain is empty")]
    ChainEmpty,
    #[error("extends chain is deeper than {MAX_EXTENDS_DEPTH} hops")]
    DepthExceeded,
    #[error("extends chain repeats agent '{urn}'")]
    Cycle { urn: String },
    #[error("agent '{child}' extends '{extends}', not the previous agent '{previous}'")]
    ParentMismatch {
        child: String,
        extends: String,
        previous: String,
    },
    #[error("agent '{urn}' extends another agent, so the chain must start at an ancestor that does not extend")]
    Unresolved { urn: String },
    #[error("skill '{urn}' is referenced at two different pins")]
    SkillVersionConflict { urn: String },
    #[error("capability '{capability}' is required by skill '{skill}' and cannot be revoked")]
    CapabilityRevokeRequired { capability: String, skill: String },
    #[error("capabilities for skill '{urn}' were not provided")]
    SkillCapabilitiesMissing { urn: String },
    #[error("parent instructions are an external artifact and cannot be {op}")]
    InstructionArtifactOpaque { op: &'static str },
    #[error("{0}")]
    Invalid(ValidationError),
}

/// True when version and digest are the same pin.
///
/// `None` matches only `None`. A floating ref and a pinned ref are different
/// pins.
pub(crate) fn same_resource_pin(left: &ResourceRef, right: &ResourceRef) -> bool {
    let version_eq = match (&left.version, &right.version) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.major == b.major
                && a.minor == b.minor
                && a.patch == b.patch
                && a.prerelease == b.prerelease
                && a.build == b.build
        }
        _ => false,
    };
    if !version_eq {
        return false;
    }
    match (&left.digest, &right.digest) {
        (None, None) => true,
        (Some(a), Some(b)) => a.algorithm == b.algorithm && a.hex_hash == b.hex_hash,
        _ => false,
    }
}

fn ref_urn(resource_ref: &ResourceRef) -> &str {
    resource_ref
        .resource_id
        .as_ref()
        .map(|id| id.urn.as_str())
        .unwrap_or("")
}

fn agent_urn(spec: &AgentSpec) -> &str {
    spec.resource_id
        .as_ref()
        .map(|id| id.urn.as_str())
        .unwrap_or("")
}

fn extends_urn(spec: &AgentSpec) -> &str {
    spec.extends.as_ref().map(ref_urn).unwrap_or("")
}

struct Accumulator {
    instructions: Option<String>,
    skills: Vec<ResourceRef>,
    granted: BTreeSet<String>,
}

/// Merge `chain` from the root (no `extends`) through each child.
///
/// `skill_capabilities` maps a skill URN to the capability tokens that skill
/// declares. The entry describes the pin that remains after the merge. A
/// missing URN fails closed. An empty list means the skill requires nothing.
pub fn merge_agent_chain(
    chain: &[AgentSpec],
    skill_capabilities: &BTreeMap<String, Vec<String>>,
) -> Result<MergedAgent, AgentMergeError> {
    if chain.is_empty() {
        return Err(AgentMergeError::ChainEmpty);
    }
    for spec in chain {
        spec.validate().map_err(AgentMergeError::Invalid)?;
    }
    check_chain_shape(chain)?;

    let mut acc = absorb_root(&chain[0]);
    for child in &chain[1..] {
        apply_child(&mut acc, child, skill_capabilities)?;
    }
    finish(acc, skill_capabilities)
}

fn check_chain_shape(chain: &[AgentSpec]) -> Result<(), AgentMergeError> {
    if chain.len() - 1 > MAX_EXTENDS_DEPTH {
        return Err(AgentMergeError::DepthExceeded);
    }
    let root_urn = agent_urn(&chain[0]);
    if chain[0].extends.is_some() {
        return Err(AgentMergeError::Unresolved {
            urn: root_urn.to_string(),
        });
    }
    let mut seen = HashSet::new();
    seen.insert(root_urn.to_string());
    let mut previous = root_urn.to_string();
    for child in &chain[1..] {
        let child_urn = agent_urn(child).to_string();
        if !seen.insert(child_urn.clone()) {
            return Err(AgentMergeError::Cycle { urn: child_urn });
        }
        let extends = extends_urn(child);
        if extends != previous {
            return Err(AgentMergeError::ParentMismatch {
                child: child_urn,
                extends: extends.to_string(),
                previous,
            });
        }
        previous = child_urn;
    }
    Ok(())
}

fn absorb_root(root: &AgentSpec) -> Accumulator {
    let instructions = match &root.instruction_source {
        Some(InstructionSource::SystemInstructions(text)) => Some(text.clone()),
        Some(InstructionSource::InstructionArtifact(_)) | None => None,
    };
    Accumulator {
        instructions,
        skills: root.skill_refs.clone(),
        granted: BTreeSet::new(),
    }
}

fn apply_child(
    acc: &mut Accumulator,
    child: &AgentSpec,
    skill_capabilities: &BTreeMap<String, Vec<String>>,
) -> Result<(), AgentMergeError> {
    apply_instructions(acc, child.overlay.as_ref())?;
    apply_skills(acc, child.overlay.as_ref())?;
    apply_capabilities(acc, child.overlay.as_ref(), skill_capabilities)
}

fn apply_instructions(
    acc: &mut Accumulator,
    overlay: Option<&AgentOverlay>,
) -> Result<(), AgentMergeError> {
    let change = overlay
        .and_then(|overlay| overlay.instructions.as_ref())
        .and_then(|instructions| instructions.change.as_ref());
    match change {
        None => {
            if acc.instructions.is_none() {
                return Err(AgentMergeError::InstructionArtifactOpaque { op: "inherit" });
            }
            Ok(())
        }
        Some(Change::Append(text)) => {
            let Some(parent) = acc.instructions.as_deref() else {
                return Err(AgentMergeError::InstructionArtifactOpaque { op: "append" });
            };
            acc.instructions = if parent.is_empty() {
                Some(text.clone())
            } else {
                Some(format!("{parent}\n\n{text}"))
            };
            Ok(())
        }
        Some(Change::Replace(text)) => {
            acc.instructions = Some(text.clone());
            Ok(())
        }
    }
}

fn apply_skills(
    acc: &mut Accumulator,
    overlay: Option<&AgentOverlay>,
) -> Result<(), AgentMergeError> {
    let Some(skills) = overlay.and_then(|overlay| overlay.skills.as_ref()) else {
        return Ok(());
    };
    for skill_ref in &skills.remove {
        let urn = ref_urn(skill_ref);
        acc.skills.retain(|existing| ref_urn(existing) != urn);
    }
    for skill_ref in &skills.add {
        let urn = ref_urn(skill_ref);
        if let Some(existing) = acc.skills.iter().find(|existing| ref_urn(existing) == urn) {
            if !same_resource_pin(existing, skill_ref) {
                return Err(AgentMergeError::SkillVersionConflict {
                    urn: urn.to_string(),
                });
            }
        } else {
            acc.skills.push(skill_ref.clone());
        }
    }
    Ok(())
}

fn apply_capabilities(
    acc: &mut Accumulator,
    overlay: Option<&AgentOverlay>,
    skill_capabilities: &BTreeMap<String, Vec<String>>,
) -> Result<(), AgentMergeError> {
    let Some(capabilities) = overlay.and_then(|overlay| overlay.capabilities.as_ref()) else {
        return Ok(());
    };
    if capabilities.grant.is_empty() && capabilities.revoke.is_empty() {
        return Ok(());
    }
    let required = required_capabilities(&acc.skills, skill_capabilities)?;
    for token in &capabilities.revoke {
        if required.contains(token) {
            let skill = skill_requiring(&acc.skills, skill_capabilities, token);
            return Err(AgentMergeError::CapabilityRevokeRequired {
                capability: token.clone(),
                skill,
            });
        }
    }
    for token in &capabilities.grant {
        acc.granted.insert(token.clone());
    }
    for token in &capabilities.revoke {
        acc.granted.remove(token);
    }
    Ok(())
}

fn finish(
    acc: Accumulator,
    skill_capabilities: &BTreeMap<String, Vec<String>>,
) -> Result<MergedAgent, AgentMergeError> {
    let mut capabilities = required_capabilities(&acc.skills, skill_capabilities)?;
    capabilities.extend(acc.granted.iter().cloned());
    Ok(MergedAgent {
        system_instructions: acc.instructions,
        skills: acc.skills,
        capabilities: capabilities.into_iter().collect(),
    })
}

fn required_capabilities(
    skills: &[ResourceRef],
    skill_capabilities: &BTreeMap<String, Vec<String>>,
) -> Result<BTreeSet<String>, AgentMergeError> {
    let mut required = BTreeSet::new();
    for skill in skills {
        let urn = ref_urn(skill);
        let declared = skill_capabilities.get(urn).ok_or_else(|| {
            AgentMergeError::SkillCapabilitiesMissing {
                urn: urn.to_string(),
            }
        })?;
        required.extend(declared.iter().cloned());
    }
    Ok(required)
}

fn skill_requiring(
    skills: &[ResourceRef],
    skill_capabilities: &BTreeMap<String, Vec<String>>,
    token: &str,
) -> String {
    skills
        .iter()
        .map(ref_urn)
        .find(|urn| {
            skill_capabilities
                .get(*urn)
                .is_some_and(|caps| caps.iter().any(|cap| cap == token))
        })
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{merge_agent_chain, ref_urn, AgentMergeError, MergedAgent};
    use crate::v1::{
        agent_spec::InstructionSource, instruction_overlay::Change, AgentOverlay, AgentSpec,
        CapabilityOverlay, Digest, InstructionOverlay, ResourceId, ResourceRef, SkillOverlay,
        Version,
    };
    use crate::validation::Validate;
    use std::collections::BTreeMap;

    const PARENT: &str = "urn:vam:agent:hub.vam.dev/acme/parent";
    const CHILD: &str = "urn:vam:agent:hub.vam.dev/acme/child";
    const GRAND: &str = "urn:vam:agent:hub.vam.dev/acme/grand";
    const SKILL_S: &str = "urn:vam:skill:hub.vam.dev/acme/search";
    const SKILL_T: &str = "urn:vam:skill:hub.vam.dev/acme/tickets";

    fn pinned(urn: &str, major: i32, hex: &str) -> ResourceRef {
        ResourceRef {
            resource_id: Some(ResourceId { urn: urn.into() }),
            version: Some(Version {
                major,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            digest: Some(Digest {
                algorithm: 1,
                hex_hash: hex.into(),
            }),
            locators: vec![],
        }
    }

    fn skill_name(urn: &str) -> ResourceRef {
        ResourceRef {
            resource_id: Some(ResourceId { urn: urn.into() }),
            ..Default::default()
        }
    }

    fn standalone(urn: &str, instructions: &str, skills: Vec<ResourceRef>) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId { urn: urn.into() }),
            display_name: "Agent".into(),
            role: "assistant".into(),
            instruction_source: Some(InstructionSource::SystemInstructions(instructions.into())),
            skill_refs: skills,
            ..Default::default()
        }
    }

    fn extending(urn: &str, parent_urn: &str, overlay: Option<AgentOverlay>) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId { urn: urn.into() }),
            display_name: "Child".into(),
            role: "assistant".into(),
            extends: Some(pinned(parent_urn, 1, "aa")),
            overlay,
            ..Default::default()
        }
    }

    fn instructions(change: Change) -> AgentOverlay {
        AgentOverlay {
            instructions: Some(InstructionOverlay {
                change: Some(change),
            }),
            ..Default::default()
        }
    }

    fn skills(add: Vec<ResourceRef>, remove: Vec<ResourceRef>) -> AgentOverlay {
        AgentOverlay {
            skills: Some(SkillOverlay { add, remove }),
            ..Default::default()
        }
    }

    fn capabilities(grant: &[&str], revoke: &[&str]) -> AgentOverlay {
        AgentOverlay {
            capabilities: Some(CapabilityOverlay {
                grant: grant.iter().map(|token| (*token).to_string()).collect(),
                revoke: revoke.iter().map(|token| (*token).to_string()).collect(),
            }),
            ..Default::default()
        }
    }

    fn caps(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(urn, tokens)| {
                (
                    (*urn).to_string(),
                    tokens.iter().map(|token| (*token).to_string()).collect(),
                )
            })
            .collect()
    }

    fn merge(chain: &[AgentSpec], declared: &BTreeMap<String, Vec<String>>) -> MergedAgent {
        merge_agent_chain(chain, declared).expect("merge")
    }

    fn constraints(spec: &AgentSpec) -> Vec<String> {
        spec.validate()
            .expect_err("invalid spec")
            .field_violations
            .into_iter()
            .map(|violation| violation.constraint)
            .collect()
    }

    #[test]
    fn append_joins_parent_text_with_a_blank_line() {
        let parent = standalone(PARENT, "Be brief.", vec![]);
        let child = extending(
            CHILD,
            PARENT,
            Some(instructions(Change::Append("Reply in Hindi.".into()))),
        );
        let merged = merge(&[parent, child], &BTreeMap::new());
        assert_eq!(
            merged.system_instructions.as_deref(),
            Some("Be brief.\n\nReply in Hindi.")
        );
    }

    #[test]
    fn replace_discards_parent_text() {
        let parent = standalone(PARENT, "Be brief.", vec![]);
        let child = extending(
            CHILD,
            PARENT,
            Some(instructions(Change::Replace("Reply only in Hindi.".into()))),
        );
        let merged = merge(&[parent, child], &BTreeMap::new());
        assert_eq!(
            merged.system_instructions.as_deref(),
            Some("Reply only in Hindi.")
        );
    }

    #[test]
    fn add_appends_a_skill_the_parent_does_not_have() {
        let parent = standalone(PARENT, "Go.", vec![pinned(SKILL_S, 1, "aa")]);
        let child = extending(
            CHILD,
            PARENT,
            Some(skills(vec![pinned(SKILL_T, 1, "bb")], vec![])),
        );
        let merged = merge(&[parent, child], &caps(&[(SKILL_S, &[]), (SKILL_T, &[])]));
        let urns: Vec<_> = merged.skills.iter().map(ref_urn).collect();
        assert_eq!(urns, vec![SKILL_S, SKILL_T]);
    }

    #[test]
    fn remove_drops_a_parent_skill_by_urn() {
        let parent = standalone(
            PARENT,
            "Go.",
            vec![pinned(SKILL_S, 1, "aa"), pinned(SKILL_T, 2, "bb")],
        );
        let child = extending(
            CHILD,
            PARENT,
            Some(skills(vec![], vec![skill_name(SKILL_S)])),
        );
        let merged = merge(&[parent, child], &caps(&[(SKILL_T, &[])]));
        assert_eq!(merged.skills.len(), 1);
        assert_eq!(ref_urn(&merged.skills[0]), SKILL_T);
    }

    #[test]
    fn grant_adds_a_capability_no_skill_declares() {
        let parent = standalone(PARENT, "Go.", vec![pinned(SKILL_S, 1, "aa")]);
        let child = extending(
            CHILD,
            PARENT,
            Some(capabilities(&["egress:example.com"], &[])),
        );
        let merged = merge(&[parent, child], &caps(&[(SKILL_S, &["tool:search"])]));
        assert_eq!(
            merged.capabilities,
            vec!["egress:example.com".to_string(), "tool:search".to_string()]
        );
    }

    #[test]
    fn revoke_removes_a_granted_capability_no_skill_requires() {
        let parent = standalone(PARENT, "Go.", vec![]);
        let child = extending(
            CHILD,
            PARENT,
            Some(capabilities(&["egress:example.com"], &[])),
        );
        let grand = extending(
            GRAND,
            CHILD,
            Some(capabilities(&[], &["egress:example.com"])),
        );
        let merged = merge(&[parent, child, grand], &BTreeMap::new());
        assert!(merged.capabilities.is_empty());
    }

    #[test]
    fn revoke_of_a_capability_the_childs_own_skill_requires_fails() {
        let parent = standalone(PARENT, "Go.", vec![]);
        let mut overlay = skills(vec![pinned(SKILL_S, 1, "aa")], vec![]);
        overlay.capabilities = Some(CapabilityOverlay {
            grant: vec![],
            revoke: vec!["tool:search".into()],
        });
        let child = extending(CHILD, PARENT, Some(overlay));
        let err = merge_agent_chain(&[parent, child], &caps(&[(SKILL_S, &["tool:search"])]))
            .expect_err("revoke under own skill");
        assert_eq!(
            err,
            AgentMergeError::CapabilityRevokeRequired {
                capability: "tool:search".into(),
                skill: SKILL_S.into(),
            }
        );
    }

    #[test]
    fn revoke_of_a_capability_an_inherited_skill_requires_fails() {
        let parent = standalone(PARENT, "Go.", vec![pinned(SKILL_S, 1, "aa")]);
        let child = extending(CHILD, PARENT, Some(capabilities(&[], &["tool:search"])));
        let err = merge_agent_chain(&[parent, child], &caps(&[(SKILL_S, &["tool:search"])]))
            .expect_err("revoke under inherited skill");
        assert_eq!(
            err,
            AgentMergeError::CapabilityRevokeRequired {
                capability: "tool:search".into(),
                skill: SKILL_S.into(),
            }
        );
    }

    #[test]
    fn diamond_skill_version_conflict_when_child_adds_a_different_pin() {
        let parent = standalone(PARENT, "Go.", vec![pinned(SKILL_S, 1, "aa")]);
        let child = extending(
            CHILD,
            PARENT,
            Some(skills(vec![pinned(SKILL_S, 2, "bb")], vec![])),
        );
        let err = merge_agent_chain(&[parent, child], &caps(&[(SKILL_S, &[])]))
            .expect_err("different pins");
        assert_eq!(
            err,
            AgentMergeError::SkillVersionConflict {
                urn: SKILL_S.into(),
            }
        );
    }

    #[test]
    fn version_conflict_when_an_intermediate_does_not_restate_the_skill() {
        let root = standalone(PARENT, "Go.", vec![pinned(SKILL_S, 1, "aa")]);
        let mid = extending(CHILD, PARENT, None);
        let grand = extending(
            GRAND,
            CHILD,
            Some(skills(vec![pinned(SKILL_S, 2, "bb")], vec![])),
        );
        let err = merge_agent_chain(&[root, mid, grand], &caps(&[(SKILL_S, &[])]))
            .expect_err("pin from the root still conflicts");
        assert_eq!(
            err,
            AgentMergeError::SkillVersionConflict {
                urn: SKILL_S.into(),
            }
        );
    }

    #[test]
    fn floating_and_pinned_adds_of_one_skill_pass_validation_and_merge_decides() {
        let parent = standalone(PARENT, "Go.", vec![]);
        let child = extending(
            CHILD,
            PARENT,
            Some(skills(
                vec![skill_name(SKILL_S), pinned(SKILL_S, 1, "aa")],
                vec![],
            )),
        );
        assert!(child.validate().is_ok());
        let err = merge_agent_chain(&[parent, child], &caps(&[(SKILL_S, &[])]))
            .expect_err("the merge compares the resolved pins");
        assert_eq!(
            err,
            AgentMergeError::SkillVersionConflict {
                urn: SKILL_S.into(),
            }
        );
    }

    #[test]
    fn floating_add_over_a_pinned_parent_skill_conflicts_at_merge_not_validation() {
        let parent = standalone(PARENT, "Go.", vec![pinned(SKILL_S, 1, "aa")]);
        let child = extending(
            CHILD,
            PARENT,
            Some(skills(vec![skill_name(SKILL_S)], vec![])),
        );
        let err = merge_agent_chain(&[parent, child], &caps(&[(SKILL_S, &[])])).expect_err("float");
        assert_eq!(
            err,
            AgentMergeError::SkillVersionConflict {
                urn: SKILL_S.into(),
            }
        );
    }

    #[test]
    fn remove_then_add_replaces_the_pin() {
        let parent = standalone(
            PARENT,
            "Go.",
            vec![pinned(SKILL_T, 1, "cc"), pinned(SKILL_S, 1, "aa")],
        );
        let child = extending(
            CHILD,
            PARENT,
            Some(skills(
                vec![pinned(SKILL_S, 2, "bb")],
                vec![skill_name(SKILL_S)],
            )),
        );
        let merged = merge(&[parent, child], &caps(&[(SKILL_S, &[]), (SKILL_T, &[])]));
        assert_eq!(merged.skills.len(), 2);
        assert_eq!(ref_urn(&merged.skills[0]), SKILL_T);
        assert_eq!(ref_urn(&merged.skills[1]), SKILL_S);
        assert_eq!(
            merged.skills[1]
                .version
                .as_ref()
                .map(|version| version.major),
            Some(2)
        );
    }

    #[test]
    fn same_pin_add_is_kept_once() {
        let parent = standalone(PARENT, "Go.", vec![pinned(SKILL_S, 1, "aa")]);
        let child = extending(
            CHILD,
            PARENT,
            Some(skills(vec![pinned(SKILL_S, 1, "aa")], vec![])),
        );
        let merged = merge(&[parent, child], &caps(&[(SKILL_S, &[])]));
        assert_eq!(merged.skills.len(), 1);
        assert_eq!(ref_urn(&merged.skills[0]), SKILL_S);
    }

    #[test]
    fn chain_of_appends_keeps_every_layer() {
        let root = standalone(PARENT, "Root.", vec![]);
        let child = extending(
            CHILD,
            PARENT,
            Some(instructions(Change::Append("Child.".into()))),
        );
        let grand = extending(
            GRAND,
            CHILD,
            Some(instructions(Change::Append("Grand.".into()))),
        );
        let merged = merge(&[root, child, grand], &BTreeMap::new());
        assert_eq!(
            merged.system_instructions.as_deref(),
            Some("Root.\n\nChild.\n\nGrand.")
        );
    }

    #[test]
    fn pure_inherit_copies_parent_instructions_and_skills() {
        let parent = standalone(PARENT, "Be brief.", vec![pinned(SKILL_S, 1, "aa")]);
        let child = extending(CHILD, PARENT, None);
        let merged = merge(&[parent, child], &caps(&[(SKILL_S, &["tool:search"])]));
        assert_eq!(merged.system_instructions.as_deref(), Some("Be brief."));
        assert_eq!(merged.skills.len(), 1);
        assert_eq!(merged.capabilities, vec!["tool:search".to_string()]);
    }

    #[test]
    fn depth_of_eight_hops_is_allowed() {
        let chain = chain_of_appends(8);
        let merged = merge(&chain, &BTreeMap::new());
        let text = merged.system_instructions.expect("inline text");
        assert!(text.contains("root"));
        assert!(text.contains("hop-8"));
        assert!(!text.contains("hop-9"));
    }

    #[test]
    fn depth_of_nine_hops_is_rejected() {
        let chain = chain_of_appends(9);
        let err = merge_agent_chain(&chain, &BTreeMap::new()).expect_err("too deep");
        assert_eq!(err, AgentMergeError::DepthExceeded);
    }

    #[test]
    fn repeated_agent_urn_is_a_cycle() {
        let root = standalone(PARENT, "Go.", vec![]);
        let mid = extending(CHILD, PARENT, None);
        let again = extending(PARENT, CHILD, None);
        let err = merge_agent_chain(&[root, mid, again], &BTreeMap::new()).expect_err("cycle");
        assert_eq!(err, AgentMergeError::Cycle { urn: PARENT.into() });
    }

    #[test]
    fn self_extends_is_rejected() {
        let mut spec = extending(CHILD, CHILD, None);
        spec.extends = Some(pinned(CHILD, 1, "aa"));
        assert!(constraints(&spec).iter().any(|c| c == "extends_cycle"));
    }

    #[test]
    fn child_that_does_not_extend_its_predecessor_is_rejected() {
        let parent = standalone(PARENT, "Go.", vec![]);
        let child = extending(CHILD, GRAND, None);
        let err = merge_agent_chain(&[parent, child], &BTreeMap::new()).expect_err("parent");
        assert_eq!(
            err,
            AgentMergeError::ParentMismatch {
                child: CHILD.into(),
                extends: GRAND.into(),
                previous: PARENT.into(),
            }
        );
    }

    #[test]
    fn child_alone_is_an_unresolved_chain() {
        let child = extending(CHILD, PARENT, None);
        let err = merge_agent_chain(&[child], &BTreeMap::new()).expect_err("no root");
        assert_eq!(err, AgentMergeError::Unresolved { urn: CHILD.into() });
    }

    #[test]
    fn empty_chain_is_rejected() {
        let err = merge_agent_chain(&[], &BTreeMap::new()).expect_err("empty");
        assert_eq!(err, AgentMergeError::ChainEmpty);
    }

    #[test]
    fn append_against_an_instruction_artifact_fails() {
        let parent = artifact_agent(PARENT);
        let child = extending(
            CHILD,
            PARENT,
            Some(instructions(Change::Append("More.".into()))),
        );
        let err = merge_agent_chain(&[parent, child], &BTreeMap::new()).expect_err("opaque");
        assert_eq!(
            err,
            AgentMergeError::InstructionArtifactOpaque { op: "append" }
        );
    }

    #[test]
    fn inherit_against_an_instruction_artifact_fails() {
        let parent = artifact_agent(PARENT);
        let child = extending(CHILD, PARENT, None);
        let err = merge_agent_chain(&[parent, child], &BTreeMap::new()).expect_err("opaque");
        assert_eq!(
            err,
            AgentMergeError::InstructionArtifactOpaque { op: "inherit" }
        );
    }

    #[test]
    fn replace_against_an_instruction_artifact_succeeds() {
        let parent = artifact_agent(PARENT);
        let child = extending(
            CHILD,
            PARENT,
            Some(instructions(Change::Replace("Inline now.".into()))),
        );
        let merged = merge(&[parent, child], &BTreeMap::new());
        assert_eq!(merged.system_instructions.as_deref(), Some("Inline now."));
    }

    #[test]
    fn revoke_succeeds_when_the_requiring_skill_was_removed() {
        let parent = standalone(PARENT, "Go.", vec![pinned(SKILL_S, 1, "aa")]);
        let mut overlay = skills(vec![], vec![skill_name(SKILL_S)]);
        overlay.capabilities = Some(CapabilityOverlay {
            grant: vec![],
            revoke: vec!["tool:search".into()],
        });
        let child = extending(CHILD, PARENT, Some(overlay));
        let merged = merge(&[parent, child], &BTreeMap::new());
        assert!(merged.skills.is_empty());
        assert!(merged.capabilities.is_empty());
    }

    #[test]
    fn missing_skill_capabilities_fail_closed() {
        let parent = standalone(PARENT, "Go.", vec![pinned(SKILL_S, 1, "aa")]);
        let err = merge_agent_chain(&[parent], &BTreeMap::new()).expect_err("missing caps");
        assert_eq!(
            err,
            AgentMergeError::SkillCapabilitiesMissing {
                urn: SKILL_S.into(),
            }
        );
    }

    #[test]
    fn grant_and_revoke_of_the_same_capability_is_rejected() {
        let spec = extending(
            CHILD,
            PARENT,
            Some(capabilities(
                &["egress:example.com"],
                &["egress:example.com"],
            )),
        );
        assert!(constraints(&spec)
            .iter()
            .any(|c| c == "capability_grant_revoke_conflict"));
    }

    #[test]
    fn two_adds_of_one_skill_at_different_pins_is_rejected() {
        let spec = extending(
            CHILD,
            PARENT,
            Some(skills(
                vec![pinned(SKILL_S, 1, "aa"), pinned(SKILL_S, 2, "bb")],
                vec![],
            )),
        );
        assert!(constraints(&spec)
            .iter()
            .any(|c| c == "skill_version_conflict"));
    }

    #[test]
    fn overlay_without_extends_is_rejected() {
        let mut spec = standalone(PARENT, "Go.", vec![]);
        spec.overlay = Some(instructions(Change::Append("More.".into())));
        assert!(constraints(&spec)
            .iter()
            .any(|c| c == "overlay_without_extends"));
    }

    #[test]
    fn floating_extends_validates_because_drafts_may_float() {
        let mut spec = extending(CHILD, PARENT, None);
        spec.extends = Some(skill_name(PARENT));
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn extends_with_a_digest_but_no_version_is_rejected() {
        let mut spec = extending(CHILD, PARENT, None);
        let mut parent_ref = pinned(PARENT, 1, "aa");
        parent_ref.version = None;
        spec.extends = Some(parent_ref);
        assert!(constraints(&spec)
            .iter()
            .any(|c| c == "digest_requires_version"));
    }

    #[test]
    fn extends_must_be_an_agent_urn() {
        let mut spec = extending(CHILD, PARENT, None);
        spec.extends = Some(pinned(SKILL_S, 1, "aa"));
        assert!(constraints(&spec).iter().any(|c| c == "agent_kind"));
    }

    #[test]
    fn skill_refs_on_an_extending_agent_are_rejected() {
        let mut spec = extending(CHILD, PARENT, None);
        spec.skill_refs = vec![pinned(SKILL_S, 1, "aa")];
        assert!(constraints(&spec)
            .iter()
            .any(|c| c == "skill_refs_not_overlay"));
    }

    #[test]
    fn instruction_source_on_an_extending_agent_is_rejected() {
        let mut spec = extending(CHILD, PARENT, None);
        spec.instruction_source = Some(InstructionSource::SystemInstructions("Own.".into()));
        assert!(constraints(&spec)
            .iter()
            .any(|c| c == "instruction_source_not_overlay"));
    }

    #[test]
    fn empty_append_is_rejected() {
        let spec = extending(
            CHILD,
            PARENT,
            Some(instructions(Change::Append("   ".into()))),
        );
        assert!(constraints(&spec).iter().any(|c| c == "instruction_empty"));
    }

    #[test]
    fn remove_of_an_absent_skill_is_a_no_op() {
        let parent = standalone(PARENT, "Go.", vec![pinned(SKILL_S, 1, "aa")]);
        let child = extending(
            CHILD,
            PARENT,
            Some(skills(vec![], vec![skill_name(SKILL_T)])),
        );
        let merged = merge(&[parent, child], &caps(&[(SKILL_S, &[])]));
        assert_eq!(merged.skills.len(), 1);
        assert_eq!(ref_urn(&merged.skills[0]), SKILL_S);
    }

    fn artifact_agent(urn: &str) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId { urn: urn.into() }),
            display_name: "Agent".into(),
            role: "assistant".into(),
            instruction_source: Some(InstructionSource::InstructionArtifact(pinned(
                "urn:vam:file:hub.vam.dev/acme/instructions",
                1,
                "cc",
            ))),
            ..Default::default()
        }
    }

    fn chain_of_appends(hops: usize) -> Vec<AgentSpec> {
        let root_urn = "urn:vam:agent:hub.vam.dev/acme/root";
        let mut chain = vec![standalone(root_urn, "root", vec![])];
        let mut previous = root_urn.to_string();
        for hop in 1..=hops {
            let urn = format!("urn:vam:agent:hub.vam.dev/acme/child-{hop}");
            chain.push(extending(
                &urn,
                &previous,
                Some(instructions(Change::Append(format!("hop-{hop}")))),
            ));
            previous = urn;
        }
        chain
    }
}
