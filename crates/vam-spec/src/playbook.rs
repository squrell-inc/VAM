//! Declared dependencies of a playbook procedure.
//!
//! Decision 16: every step has prose, an optional tool binding, and a mode of
//! `bound`, `assisted`, or `agentic`. `PlaybookSpec.requires` names the skills
//! and capabilities that procedure needs. This module reads that declaration.
//! It does not install skills, walk steps, or schedule them.

use crate::v1::{PlaybookSpec, ResourceRef};

/// Skill refs declared on `playbook.requires`, in declaration order.
///
/// An absent `requires` is an empty list. Callers pin these refs the same way
/// they pin `AgentSpec.skill_refs`.
pub fn required_skill_refs(playbook: &PlaybookSpec) -> &[ResourceRef] {
    playbook
        .requires
        .as_ref()
        .map(|requires| requires.skills.as_slice())
        .unwrap_or(&[])
}

/// Rewrite a v1 playbook into spec v2 before it is stored.
///
/// Each `depends_on` entry is copied onto `after` as a bare step id, then
/// cleared. An unspecified step `kind` becomes `task`. Retired fields are left
/// in place so publish validation can reject them.
pub fn migrate_playbook_spec(spec: &mut PlaybookSpec) {
    for step in &mut spec.steps {
        if step.kind == crate::v1::StepKind::Unspecified as i32 {
            step.kind = crate::v1::StepKind::Task as i32;
        }
        if step.depends_on.is_empty() {
            continue;
        }
        let migrated = std::mem::take(&mut step.depends_on);
        for dep in migrated {
            if !step.after.iter().any(|existing| existing == &dep) {
                step.after.push(dep);
            }
        }
    }
}

/// First words of a successful `start_playbook` result. The guest reads it to
/// know a playbook is now bound to the task; the rest is the procedure.
pub const PLAYBOOK_STARTED_PREFIX: &str = "Started playbook ";

/// First words of a successful `start_playbook` for a guidance-only playbook.
/// Nothing is bound to the task: the rest of the result is the guidance text.
pub const PLAYBOOK_GUIDANCE_PREFIX: &str = "Playbook guidance ";

/// Longest `PlaybookSpec.guidance`, in characters.
pub const GUIDANCE_MAX_CHARS: usize = 8000;

/// The procedure rule for the model (decision 21, G7).
pub const PLAYBOOK_GATE_RULE: &str = "A gate collects information from the user. When a step needs a value from the user, wait for its gate; do not ask for it in chat and never invent, guess or fill in values. Use only values in the status block's Captured list or in tool results. After each step read the status block and continue with an available step.";

impl crate::v1::PlaybookActivation {
    /// An `ask`-mode activation with the given `when`. For callers that build a
    /// playbook in code, since publish requires `activation.when`.
    pub fn asking(when: impl Into<String>) -> Self {
        Self {
            when: when.into(),
            examples: Vec::new(),
            mode: crate::v1::ActivationMode::Ask as i32,
        }
    }
}
