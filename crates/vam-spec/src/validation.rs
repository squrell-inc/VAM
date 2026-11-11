use crate::v1::{
    approval_decision_claims, ActivationMode, AgentSpec, ApprovalDecisionClaims,
    ApprovalRequestClaims, ArtifactRef, CapabilityManifest, Cost, Digest, ExecutionLease, FileRef,
    HookPhase, KnowledgeRef, OutcomeTerminal, PlaybookGuard, PlaybookSpec, PlaybookStep,
    RegisterRequest, RegistryCoordinates, ResourceId, ResourceLocator, ResourceRef,
    RunRequestPermission, RunSpec, SideEffectClass, SignedEnvelope, SkillChannel, SkillManifest,
    SkillPackageRecord, StepKind, StepMode, TaskOutcome, TaskOutcomeState, TokenUsage,
    ToolDestination, ToolSpec, TrustTier, Version,
};
use sha2::{Digest as Sha2Digest, Sha256};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub field_violations: Vec<crate::v1::ValidationViolation>,
}

impl ValidationError {
    pub fn new(
        field_path: impl Into<String>,
        description: impl Into<String>,
        constraint: impl Into<String>,
    ) -> Self {
        Self {
            field_violations: vec![crate::v1::ValidationViolation {
                field_path: field_path.into(),
                description: description.into(),
                constraint: constraint.into(),
            }],
        }
    }

    pub fn empty() -> Self {
        Self {
            field_violations: Vec::new(),
        }
    }

    pub fn push(
        &mut self,
        field_path: impl Into<String>,
        description: impl Into<String>,
        constraint: impl Into<String>,
    ) {
        self.field_violations.push(crate::v1::ValidationViolation {
            field_path: field_path.into(),
            description: description.into(),
            constraint: constraint.into(),
        });
    }

    pub fn merge(&mut self, other: ValidationError) {
        self.field_violations.extend(other.field_violations);
    }

    pub fn prefix_path(&mut self, prefix: &str) {
        for violation in &mut self.field_violations {
            if violation.field_path.is_empty() {
                violation.field_path = prefix.to_string();
            } else if violation.field_path.starts_with('[') {
                violation.field_path = format!("{}{}", prefix, violation.field_path);
            } else {
                violation.field_path = format!("{}.{}", prefix, violation.field_path);
            }
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Validation failed with {} violation(s):",
            self.field_violations.len()
        )?;
        for violation in &self.field_violations {
            write!(
                f,
                "\n  - {}: {} (constraint: {})",
                violation.field_path, violation.description, violation.constraint
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationError {}

pub trait Validate {
    fn validate(&self) -> Result<(), ValidationError>;
}

impl Validate for ResourceId {
    fn validate(&self) -> Result<(), ValidationError> {
        let Some((kind, path)) = self
            .urn
            .strip_prefix("urn:vam:")
            .and_then(|value| value.split_once(':'))
        else {
            return Err(ValidationError::new(
                "urn",
                format!(
                    "URN must match 'urn:vam:<kind>:<registry>/<namespace>/<name>', got '{}'",
                    self.urn
                ),
                "pattern:urn:vam:<kind>:<registry>/<namespace>/<name>",
            ));
        };

        if !is_slug(kind) {
            return Err(ValidationError::new(
                "urn",
                format!("URN resource kind is invalid: '{kind}'"),
                "is_slug",
            ));
        }

        let segments: Vec<_> = path.split('/').collect();
        if segments.len() < 3 || segments.iter().any(|segment| !is_slug(segment)) {
            return Err(ValidationError::new(
                "urn",
                format!("URN path must contain a registry, namespace, and name using lowercase normalized segments, got '{path}'"),
                "pattern:registry/namespace/name",
            ));
        }

        Ok(())
    }
}

impl Validate for RegistryCoordinates {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();

        if !is_registry(&self.registry) {
            errors.push(
                "registry",
                format!("Registry is invalid: '{}'", self.registry),
                "is_registry",
            );
        }

        if self.namespace.is_empty() || self.namespace.split('/').any(|segment| !is_slug(segment)) {
            errors.push(
                "namespace",
                format!("Namespace is invalid: '{}'", self.namespace),
                "is_slug_segments",
            );
        }

        if !is_slug(&self.name) {
            errors.push(
                "name",
                format!("Resource name is invalid: '{}'", self.name),
                "is_slug",
            );
        }

        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for ResourceLocator {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();

        if !is_slug(&self.provider) {
            errors.push(
                "provider",
                format!("Locator provider is invalid: '{}'", self.provider),
                "is_slug",
            );
        }
        if self.uri.trim().is_empty() {
            errors.push("uri", "Locator URI cannot be empty", "required");
        }
        if let Some(coordinates) = &self.coordinates {
            if let Err(mut err) = coordinates.validate() {
                err.prefix_path("coordinates");
                errors.merge(err);
            }
        }

        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for ResourceRef {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();

        if let Some(resource_id) = &self.resource_id {
            if let Err(mut err) = resource_id.validate() {
                err.prefix_path("resource_id");
                errors.merge(err);
            }
        } else {
            errors.push(
                "resource_id",
                "ResourceRef.resource_id is required",
                "required",
            );
        }

        if let Some(version) = &self.version {
            if let Err(mut err) = version.validate() {
                err.prefix_path("version");
                errors.merge(err);
            }
        } else {
            errors.push("version", "ResourceRef.version is required", "required");
        }

        if let Some(digest) = &self.digest {
            if let Err(mut err) = digest.validate() {
                err.prefix_path("digest");
                errors.merge(err);
            }
        } else {
            errors.push("digest", "ResourceRef.digest is required", "required");
        }

        for (i, locator) in self.locators.iter().enumerate() {
            if let Err(mut err) = locator.validate() {
                err.prefix_path(&format!("locators[{}]", i));
                errors.merge(err);
            }
        }

        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// The kind segment of a `urn:vam:<kind>:...` URN, when the URN has that shape.
fn urn_kind(urn: &str) -> Option<&str> {
    urn.strip_prefix("urn:vam:")?
        .split_once(':')
        .map(|(kind, _)| kind)
}

/// Authored reference rule (AX-3.1). An author writes a URN, optionally with a
/// version, and never a digest without a version:
///
/// * URN only: the ref floats. The server resolves it at run time.
/// * URN plus version: no digest required.
/// * URN plus version plus digest: a full pin. The server checks the digest at
///   resolution.
///
/// This is the authored rule. It does not require the URN to be of any kind;
/// callers that need a kind use [`validate_kinded_ref`].
pub fn validate_resolvable_ref(resource_ref: &ResourceRef) -> Result<(), ValidationError> {
    let mut errors = ValidationError::empty();
    if let Some(resource_id) = &resource_ref.resource_id {
        if let Err(mut err) = resource_id.validate() {
            err.prefix_path("resource_id");
            errors.merge(err);
        }
    } else {
        errors.push(
            "resource_id",
            "ResourceRef.resource_id is required",
            "required",
        );
    }
    if let Some(version) = &resource_ref.version {
        if let Err(mut err) = version.validate() {
            err.prefix_path("version");
            errors.merge(err);
        }
    }
    if let Some(digest) = &resource_ref.digest {
        if resource_ref.version.is_none() {
            errors.push(
                "digest",
                "ResourceRef.digest requires a version: a digest pins one version",
                "digest_requires_version",
            );
        }
        if let Err(mut err) = digest.validate() {
            err.prefix_path("digest");
            errors.merge(err);
        }
    }
    for (i, locator) in resource_ref.locators.iter().enumerate() {
        if let Err(mut err) = locator.validate() {
            err.prefix_path(&format!("locators[{}]", i));
            errors.merge(err);
        }
    }
    if errors.field_violations.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// [`validate_resolvable_ref`] plus a check that the URN names `kind`, such as
/// `policy` or `playbook`. The constraint is `<kind>_kind`.
fn validate_kinded_ref(
    resource_ref: &ResourceRef,
    path: &str,
    kind: &str,
    errors: &mut ValidationError,
) {
    if let Err(mut err) = validate_resolvable_ref(resource_ref) {
        err.prefix_path(path);
        errors.merge(err);
    }
    let urn = resource_ref
        .resource_id
        .as_ref()
        .map(|id| id.urn.as_str())
        .unwrap_or("");
    if let Some(found) = urn_kind(urn) {
        if found != kind {
            errors.push(
                path,
                format!("expected a '{kind}' URN, got '{urn}'"),
                format!("{kind}_kind"),
            );
        }
    }
}

impl Validate for PlaybookSpec {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();

        if let Some(resource_id) = &self.resource_id {
            if let Err(mut err) = resource_id.validate() {
                err.prefix_path("resource_id");
                errors.merge(err);
            }
        } else {
            errors.push(
                "resource_id",
                "PlaybookSpec.resource_id is required",
                "required",
            );
        }

        let ids: HashSet<_> = self.steps.iter().map(|step| step.id.as_str()).collect();
        if ids.contains("") {
            errors.push("steps", "Playbook step IDs must not be empty", "non_empty");
        }
        if ids.len() != self.steps.len() {
            errors.push("steps", "Playbook step IDs must be unique", "unique");
        }

        let guidance = self.guidance.trim();
        if self.steps.is_empty() {
            if guidance.is_empty() {
                errors.push(
                    "steps",
                    "Playbooks must contain at least one step, or be guidance-only (no steps and a non-empty `guidance`)",
                    "min_items_1",
                );
            }
            if self.output_schema.is_some() {
                errors.push(
                    "output_schema",
                    "a guidance-only playbook tracks nothing and cannot declare an output_schema",
                    "guidance_output_schema",
                );
            }
        } else if !self.guidance.is_empty() {
            errors.push(
                "guidance",
                "`guidance` is only for playbooks without steps; put prose in the steps' instructions",
                "guidance_with_steps",
            );
        }
        if self.guidance.chars().count() > crate::playbook::GUIDANCE_MAX_CHARS {
            errors.push(
                "guidance",
                format!(
                    "guidance must be at most {} characters",
                    crate::playbook::GUIDANCE_MAX_CHARS
                ),
                "max_len",
            );
        }

        let ids_ok = !ids.contains("") && ids.len() == self.steps.len();
        if ids_ok && !self.steps.is_empty() {
            validate_playbook_steps(self, &ids, &mut errors);
        }

        if let Some(requires) = &self.requires {
            let mut seen_skills = HashSet::new();
            for (i, skill_ref) in requires.skills.iter().enumerate() {
                let path = format!("requires.skills[{i}]");
                if let Err(mut err) = validate_resolvable_ref(skill_ref) {
                    err.prefix_path(&path);
                    errors.merge(err);
                }
                let urn = skill_ref
                    .resource_id
                    .as_ref()
                    .map(|id| id.urn.as_str())
                    .unwrap_or("");
                if !urn.is_empty() && !urn.contains(":skill:") {
                    errors.push(
                        &path,
                        "PlaybookSpec.requires.skills entries must be skill URNs",
                        "skill_kind",
                    );
                }
                if !urn.is_empty() && !seen_skills.insert(urn) {
                    errors.push(&path, format!("duplicate skill ref '{urn}'"), "unique");
                }
            }
            for (i, capability) in requires.capabilities.iter().enumerate() {
                if let Err(reason) = validate_capability_token(capability) {
                    errors.push(
                        format!("requires.capabilities[{i}]"),
                        reason,
                        "capability_shape",
                    );
                }
            }
        }

        validate_playbook_activation(self, &mut errors);

        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Longest `activation.when`, in characters (one or two sentences).
pub const ACTIVATION_WHEN_MAX_CHARS: usize = 280;
/// Most `activation.examples` entries.
pub const ACTIVATION_EXAMPLES_MAX: usize = 5;
/// Longest single `activation.examples` entry, in characters.
pub const ACTIVATION_EXAMPLE_MAX_CHARS: usize = 200;

/// Builtins a step may bind under `auto` activation: they read and never write.
const AUTO_SAFE_BUILTINS: &[&str] = &["read_file", "glob", "grep", "load_skill"];

/// Shape checks for `activation`. Absent activation is fine here: a draft may
/// be saved without it. [`PlaybookSpec::validate_for_publish`] requires `when`.
fn validate_playbook_activation(spec: &PlaybookSpec, errors: &mut ValidationError) {
    let Some(activation) = &spec.activation else {
        return;
    };
    let when = activation.when.trim();
    if activation.when.chars().count() > ACTIVATION_WHEN_MAX_CHARS {
        errors.push(
            "activation.when",
            format!("activation.when must be at most {ACTIVATION_WHEN_MAX_CHARS} characters"),
            "max_len",
        );
    }
    if when.is_empty() && !activation.examples.is_empty() {
        errors.push(
            "activation.examples",
            "activation.examples need an activation.when",
            "activation_when_required",
        );
    }
    if activation.examples.len() > ACTIVATION_EXAMPLES_MAX {
        errors.push(
            "activation.examples",
            format!("activation.examples allows at most {ACTIVATION_EXAMPLES_MAX} entries"),
            "max_items",
        );
    }
    for (index, example) in activation.examples.iter().enumerate() {
        let path = format!("activation.examples[{index}]");
        if example.trim().is_empty() {
            errors.push(&path, "an example must not be empty", "non_empty");
        } else if example.chars().count() > ACTIVATION_EXAMPLE_MAX_CHARS {
            errors.push(
                &path,
                format!("an example must be at most {ACTIVATION_EXAMPLE_MAX_CHARS} characters"),
                "max_len",
            );
        }
    }
    let mode = ActivationMode::try_from(activation.mode);
    if mode.is_err() {
        errors.push(
            "activation.mode",
            "activation.mode must be ask or auto",
            "activation_mode",
        );
    }
    if mode == Ok(ActivationMode::Auto) {
        for (index, step) in spec.steps.iter().enumerate() {
            let Some(binding) = &step.binding else {
                continue;
            };
            let tool = binding.tool_name.as_str();
            if is_reserved_builtin_name(tool) && !AUTO_SAFE_BUILTINS.contains(&tool) {
                errors.push(
                    format!("steps[{index}].binding.tool_name"),
                    format!(
                        "activation.mode auto is refused: step '{}' uses the mutating tool '{tool}'. Use mode ask.",
                        step.id
                    ),
                    "activation_auto_mutating",
                );
            }
        }
    }
}

impl PlaybookSpec {
    /// Everything [`Validate::validate`] checks, plus the rules that apply only
    /// when a playbook is published: `activation.when` is required.
    pub fn validate_for_publish(&self) -> Result<(), ValidationError> {
        let mut errors = match self.validate() {
            Ok(()) => ValidationError::empty(),
            Err(errors) => errors,
        };
        let has_when = self
            .activation
            .as_ref()
            .is_some_and(|activation| !activation.when.trim().is_empty());
        if !has_when {
            errors.push(
                "activation.when",
                "playbook.activation.when is required: one or two sentences saying when the model should start this playbook (for example \"Use when the user wants to draft a pull request\"). Add it and publish again.",
                "activation_when_required",
            );
        }
        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for ArtifactRef {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();
        if let Some(resource_id) = &self.artifact_id {
            if let Err(mut err) = resource_id.validate() {
                err.prefix_path("artifact_id");
                errors.merge(err);
            }
        } else {
            errors.push(
                "artifact_id",
                "ArtifactRef.artifact_id is required",
                "required",
            );
        }
        if let Some(digest) = &self.digest {
            if let Err(mut err) = digest.validate() {
                err.prefix_path("digest");
                errors.merge(err);
            }
        } else {
            errors.push("digest", "ArtifactRef.digest is required", "required");
        }
        if self.size_bytes < 0 {
            errors.push("size_bytes", "Artifact size must be non-negative", "min_0");
        }
        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for FileRef {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();
        if let Some(file_id) = &self.file_id {
            if let Err(mut err) = file_id.validate() {
                err.prefix_path("file_id");
                errors.merge(err);
            } else if !file_id.urn.contains(":file:") {
                errors.push(
                    "file_id",
                    "FileRef file_id URN must be of kind 'file'",
                    "file_kind",
                );
            }
        } else {
            errors.push("file_id", "FileRef.file_id is required", "required");
        }
        if let Some(digest) = &self.digest {
            if let Err(mut err) = digest.validate() {
                err.prefix_path("digest");
                errors.merge(err);
            }
        } else {
            errors.push("digest", "FileRef.digest is required", "required");
        }
        if self.size_bytes < 0 {
            errors.push("size_bytes", "FileRef size must be non-negative", "min_0");
        }
        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for KnowledgeRef {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();
        if let Some(knowledge_id) = &self.knowledge_id {
            if let Err(mut err) = knowledge_id.validate() {
                err.prefix_path("knowledge_id");
                errors.merge(err);
            } else if !knowledge_id.urn.contains(":knowledge:") {
                errors.push(
                    "knowledge_id",
                    "KnowledgeRef knowledge_id URN must be of kind 'knowledge'",
                    "knowledge_kind",
                );
            }
        } else {
            errors.push(
                "knowledge_id",
                "KnowledgeRef.knowledge_id is required",
                "required",
            );
        }
        if let Some(digest) = &self.digest {
            if let Err(mut err) = digest.validate() {
                err.prefix_path("digest");
                errors.merge(err);
            }
        }
        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Gateway rate limit applied when `max_requests_per_minute` is 0.
pub const DEFAULT_RUN_REQUESTS_PER_MINUTE: i32 = 60;
/// Gateway rate limit applied when `max_requests_per_day` is 0.
pub const DEFAULT_RUN_REQUESTS_PER_DAY: i32 = 1440;
/// Active grants per owner and broker when `max_active_grants_per_user` is 0.
pub const DEFAULT_MAX_ACTIVE_GRANTS_PER_USER: i32 = 50;
/// Receipt surfaces a broker may not claim.
pub const RESERVED_RUN_SURFACES: &[&str] = &["cli", "app", "api", "system"];

/// The limits of a `RunRequestPermission` with 0 replaced by the node default.
/// Returns (per minute, per day, active grants per user).
pub fn effective_run_request_limits(permission: &RunRequestPermission) -> (i32, i32, i32) {
    let or_default = |value: i32, default: i32| if value == 0 { default } else { value };
    (
        or_default(
            permission.max_requests_per_minute,
            DEFAULT_RUN_REQUESTS_PER_MINUTE,
        ),
        or_default(
            permission.max_requests_per_day,
            DEFAULT_RUN_REQUESTS_PER_DAY,
        ),
        or_default(
            permission.max_active_grants_per_user,
            DEFAULT_MAX_ACTIVE_GRANTS_PER_USER,
        ),
    )
}

fn validate_run_request_permission(
    permission: &RunRequestPermission,
    tool_names: &HashSet<&str>,
    third_party: bool,
    errors: &mut ValidationError,
) {
    const PATH: &str = "run_request_permission";
    if third_party {
        errors.push(
            PATH,
            "a THIRD_PARTY manifest may not declare run_request_permission",
            "run_request_permission_tier",
        );
    }
    if permission.grant_tools.is_empty() {
        errors.push(
            format!("{PATH}.grant_tools"),
            "run_request_permission must name at least one grant tool",
            "required",
        );
    }
    for (i, tool) in permission.grant_tools.iter().enumerate() {
        if !tool_names.contains(tool.as_str()) {
            errors.push(
                format!("{PATH}.grant_tools[{i}]"),
                format!("grant tool '{tool}' is not an exported tool"),
                "grant_tools_subset",
            );
        }
    }
    for (i, tool) in permission.loop_guarded_tools.iter().enumerate() {
        if !tool_names.contains(tool.as_str()) {
            errors.push(
                format!("{PATH}.loop_guarded_tools[{i}]"),
                format!("loop-guarded tool '{tool}' is not an exported tool"),
                "loop_guarded_subset",
            );
        }
    }
    for (i, tool) in permission.grant_tools.iter().enumerate() {
        if !permission.loop_guarded_tools.contains(tool) {
            errors.push(
                format!("{PATH}.grant_tools[{i}]"),
                format!("grant tool '{tool}' must also be in loop_guarded_tools"),
                "loop_guarded_superset",
            );
        }
    }
    let surface = permission.surface.as_str();
    let charset_ok =
        (3..=16).contains(&surface.len()) && surface.bytes().all(|b| b.is_ascii_lowercase());
    if !charset_ok {
        errors.push(
            format!("{PATH}.surface"),
            "surface must be 3 to 16 lowercase letters [a-z]",
            "surface_charset",
        );
    } else if RESERVED_RUN_SURFACES.contains(&surface) {
        errors.push(
            format!("{PATH}.surface"),
            format!("surface '{surface}' is reserved"),
            "surface_reserved",
        );
    }
    for (name, value) in [
        (
            "max_requests_per_minute",
            permission.max_requests_per_minute,
        ),
        ("max_requests_per_day", permission.max_requests_per_day),
        (
            "max_active_grants_per_user",
            permission.max_active_grants_per_user,
        ),
    ] {
        if value < 0 {
            errors.push(
                format!("{PATH}.{name}"),
                format!("{name} cannot be negative"),
                "range",
            );
        }
    }
}

/// Channels a notification broker may declare.
pub const NOTIFICATION_CHANNELS: &[&str] = &["slack"];

/// Decisions a channel may send besides a gate option id.
pub const CHANNEL_DECISIONS: &[&str] = &["APPROVED", "REJECTED", "SUBMITTED"];

fn validate_notification_channel(
    channel: &crate::v1::NotificationChannel,
    trust_tier: i32,
    errors: &mut ValidationError,
) {
    const PATH: &str = "notification_channel";
    if trust_tier != TrustTier::Core as i32 && trust_tier != TrustTier::FirstParty as i32 {
        errors.push(
            PATH,
            "only a CORE or FIRST_PARTY manifest may declare notification_channel",
            "notification_channel_tier",
        );
    }
    if !NOTIFICATION_CHANNELS.contains(&channel.channel.as_str()) {
        errors.push(
            format!("{PATH}.channel"),
            format!("channel '{}' is not supported", channel.channel),
            "notification_channel_unknown",
        );
    }
    let surface = channel.surface.as_str();
    let charset_ok =
        (3..=16).contains(&surface.len()) && surface.bytes().all(|b| b.is_ascii_lowercase());
    if !charset_ok {
        errors.push(
            format!("{PATH}.surface"),
            "surface must be 3 to 16 lowercase letters [a-z]",
            "surface_charset",
        );
    } else if RESERVED_RUN_SURFACES.contains(&surface) {
        errors.push(
            format!("{PATH}.surface"),
            format!("surface '{surface}' is reserved"),
            "surface_reserved",
        );
    }
    if channel.max_actions_per_minute < 0 {
        errors.push(
            format!("{PATH}.max_actions_per_minute"),
            "max_actions_per_minute cannot be negative",
            "range",
        );
    }
}

/// The closed decision set of `ActOnNotificationRequest.decision`: `APPROVED`, `REJECTED`,
/// `SUBMITTED`, or one of the gate option ids vamd sent in the rendering (`offered`).
/// Empty, lowercase variants, `AMENDED` and any "remember" or "always" word are refused, even
/// when a hostile option id spells one.
pub fn validate_channel_decision(decision: &str, offered: &[&str]) -> Result<(), String> {
    if decision.is_empty() {
        return Err("decision is empty".into());
    }
    let lower = decision.to_ascii_lowercase();
    if lower == "amended" || lower.contains("remember") || lower.contains("always") {
        return Err(format!(
            "decision '{decision}' is not allowed from a channel"
        ));
    }
    if CHANNEL_DECISIONS.contains(&decision) || offered.contains(&decision) {
        Ok(())
    } else {
        Err(format!("decision '{decision}' was not offered"))
    }
}

/// Checks a tool's `effect` declaration: a known class, `read` only on a read-only tool, and a
/// verify read that names another read-only tool of the same manifest and carries a `found` rule.
/// `side_effect_of` gives the side-effect class of a sibling tool by name.
fn validate_effect(
    errors: &mut ValidationError,
    path: &str,
    tool_name: &str,
    side_effect: i32,
    effect: Option<&crate::v1::EffectSpec>,
    side_effect_of: impl Fn(&str) -> Option<i32>,
) {
    use crate::v1::EffectClass;
    let Some(effect) = effect else {
        return;
    };
    match EffectClass::try_from(effect.class) {
        Err(_) => errors.push(
            format!("{path}.effect.class"),
            "unknown effect class",
            "effect_class",
        ),
        Ok(EffectClass::Read) if side_effect != SideEffectClass::ReadOnly as i32 => errors.push(
            format!("{path}.effect.class"),
            "effect class READ is only allowed on a READ_ONLY tool",
            "effect_class",
        ),
        Ok(_) => {}
    }
    let Some(verify) = &effect.verify else {
        return;
    };
    let vpath = format!("{path}.effect.verify");
    if verify.tool == tool_name {
        errors.push(
            format!("{vpath}.tool"),
            "a verify read cannot be the tool itself",
            "effect_verify",
        );
    } else if side_effect_of(&verify.tool) != Some(SideEffectClass::ReadOnly as i32) {
        errors.push(
            format!("{vpath}.tool"),
            format!(
                "verify tool '{}' must be a READ_ONLY tool of the same manifest",
                verify.tool
            ),
            "effect_verify",
        );
    }
    match &verify.found {
        None => errors.push(
            format!("{vpath}.found"),
            "a verify read needs a found rule",
            "required",
        ),
        Some(found) => {
            if found.path.trim().is_empty() {
                errors.push(
                    format!("{vpath}.found.path"),
                    "found.path is required",
                    "required",
                );
            }
            if found.r#match.is_none() {
                errors.push(
                    format!("{vpath}.found"),
                    "found needs non_empty or equals",
                    "required",
                );
            }
        }
    }
}

impl Validate for CapabilityManifest {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();

        if !is_broker_id(&self.broker_id) {
            errors.push(
                "broker_id",
                format!(
                    "CapabilityManifest broker_id must be a non-empty slug, got '{}'",
                    self.broker_id
                ),
                "is_broker_id",
            );
        }

        match TrustTier::try_from(self.trust_tier) {
            Ok(TrustTier::Unspecified) | Err(_) => {
                errors.push("trust_tier", "trust_tier cannot be unspecified", "required");
            }
            Ok(_) => {}
        }

        let mut tool_names = HashSet::new();
        for (i, tool) in self.tools.iter().enumerate() {
            if tool.tool_name.trim().is_empty() {
                errors.push(
                    format!("tools[{}].tool_name", i),
                    "ToolExport tool_name cannot be empty",
                    "required",
                );
            } else if !tool_names.insert(tool.tool_name.as_str()) {
                errors.push(
                    format!("tools[{}]", i),
                    format!(
                        "Duplicate tool name '{}' within the capability manifest",
                        tool.tool_name
                    ),
                    "unique_tool_names",
                );
            }
            validate_effect(
                &mut errors,
                &format!("tools[{}]", i),
                &tool.tool_name,
                tool.side_effect,
                tool.effect.as_ref(),
                |name| {
                    self.tools
                        .iter()
                        .find(|t| t.tool_name == name)
                        .map(|t| t.side_effect)
                },
            );
            if tool.destination_policy == ToolDestination::PublicWeb as i32 {
                let tier_ok = matches!(
                    TrustTier::try_from(self.trust_tier),
                    Ok(TrustTier::FirstParty) | Ok(TrustTier::Operator)
                );
                if SideEffectClass::try_from(tool.side_effect) != Ok(SideEffectClass::ReadOnly) {
                    errors.push(
                        format!("tools[{}].destination_policy", i),
                        "PUBLIC_WEB is only allowed on a READ_ONLY tool export",
                        "destination_policy",
                    );
                }
                if !tier_ok {
                    errors.push(
                        format!("tools[{}].destination_policy", i),
                        "PUBLIC_WEB is only allowed on a FIRST_PARTY or OPERATOR manifest",
                        "destination_policy",
                    );
                }
            } else if ToolDestination::try_from(tool.destination_policy).is_err() {
                errors.push(
                    format!("tools[{}].destination_policy", i),
                    "unknown destination_policy",
                    "destination_policy",
                );
            }
        }

        for (i, binding) in self.secret_bindings.iter().enumerate() {
            if binding.logical_name.trim().is_empty() {
                errors.push(
                    format!("secret_bindings[{}].logical_name", i),
                    "SecretBinding logical_name cannot be empty",
                    "required",
                );
            }
            if binding.required && binding.vault_key.trim().is_empty() {
                errors.push(
                    format!("secret_bindings[{}].vault_key", i),
                    "hsp denied: unbound secret",
                    "required",
                );
            }
        }

        if let Some(permission) = &self.run_request_permission {
            validate_run_request_permission(
                permission,
                &tool_names,
                self.trust_tier == TrustTier::ThirdParty as i32,
                &mut errors,
            );
        }

        if let Some(channel) = &self.notification_channel {
            validate_notification_channel(channel, self.trust_tier, &mut errors);
        }

        let mut hook_phases = HashSet::new();
        for (i, hook) in self.hooks.iter().enumerate() {
            let phase = HookPhase::try_from(hook.phase).unwrap_or(HookPhase::Unspecified);
            if phase == HookPhase::Unspecified {
                errors.push(
                    format!("hooks[{}].phase", i),
                    "HookParticipation phase is required",
                    "required",
                );
            } else if !hook_phases.insert(phase) {
                errors.push(
                    format!("hooks[{}]", i),
                    "Duplicate hook phase in capability manifest",
                    "unique_hook_phases",
                );
            }
        }

        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for RegisterRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();

        if !is_broker_id(&self.broker_id) {
            errors.push(
                "broker_id",
                format!(
                    "RegisterRequest broker_id must be a non-empty slug, got '{}'",
                    self.broker_id
                ),
                "is_broker_id",
            );
        }

        if let Some(manifest) = &self.manifest {
            if let Err(mut err) = manifest.validate() {
                err.prefix_path("manifest");
                errors.merge(err);
            }
            if !self.broker_id.is_empty()
                && !manifest.broker_id.is_empty()
                && self.broker_id != manifest.broker_id
            {
                errors.push(
                    "broker_id",
                    "RegisterRequest broker_id must match manifest.broker_id",
                    "broker_id_match",
                );
            }
        } else {
            errors.push(
                "manifest",
                "RegisterRequest manifest is required",
                "required",
            );
        }

        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for ExecutionLease {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();
        for (field, value) in [
            ("step_id", self.step_id.as_str()),
            ("idempotency_key", self.idempotency_key.as_str()),
        ] {
            if value.is_empty() {
                errors.push(
                    field,
                    format!("ExecutionLease.{} is required", field),
                    "required",
                );
            }
        }
        for (field, resource_id) in [
            ("run_id", &self.run_id),
            ("attempt_id", &self.attempt_id),
            ("lease_id", &self.lease_id),
        ] {
            if let Some(resource_id) = resource_id {
                if let Err(mut err) = resource_id.validate() {
                    err.prefix_path(field);
                    errors.merge(err);
                }
            } else {
                errors.push(
                    field,
                    format!("ExecutionLease.{} is required", field),
                    "required",
                );
            }
        }
        if self.lifecycle_generation <= 0 {
            errors.push(
                "lifecycle_generation",
                "ExecutionLease.lifecycle_generation must be positive",
                "min_1",
            );
        }
        if self.fencing_token.is_empty() {
            errors.push(
                "fencing_token",
                "ExecutionLease.fencing_token is required",
                "required",
            );
        }
        if self.expires_at.is_none() {
            errors.push(
                "expires_at",
                "ExecutionLease.expires_at is required",
                "required",
            );
        }
        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for TaskOutcome {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();
        if let Some(lease) = &self.lease {
            if let Err(mut err) = lease.validate() {
                err.prefix_path("lease");
                errors.merge(err);
            }
        } else {
            errors.push("lease", "TaskOutcome.lease is required", "required");
        }
        let state = TaskOutcomeState::try_from(self.state).unwrap_or(TaskOutcomeState::Unspecified);
        if state == TaskOutcomeState::Unspecified {
            errors.push("state", "TaskOutcome.state is required", "required");
        }
        if state == TaskOutcomeState::Suspended && self.continuation.is_none() {
            errors.push(
                "continuation",
                "Suspended task outcomes require a continuation artifact",
                "required_for_suspension",
            );
        }
        if matches!(
            state,
            TaskOutcomeState::Failed | TaskOutcomeState::Cancelled
        ) && self.error.is_none()
        {
            errors.push(
                "error",
                "Failed and cancelled task outcomes require an error",
                "required_for_terminal_failure",
            );
        }
        for (index, artifact) in self.output_artifacts.iter().enumerate() {
            if let Err(mut err) = artifact.validate() {
                err.prefix_path(&format!("output_artifacts[{}]", index));
                errors.merge(err);
            }
        }
        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Digest of a bound step's literal arguments, so the exact call is
/// inspectable before the playbook runs. `None` for assisted/agentic steps,
/// unbound steps, and arguments that contain `{{…}}` template syntax (the host
/// resolves those at run time and digests the resolved arguments).
pub fn bound_step_args_digest(step: &crate::v1::PlaybookStep) -> Option<String> {
    if StepMode::try_from(step.mode).ok()? != StepMode::Bound {
        return None;
    }
    let arguments = step.binding.as_ref()?.arguments.as_ref();
    if arguments.and_then(find_template_field).is_some() {
        return None;
    }
    crate::canonical_args_digest(arguments).ok()
}

fn find_template_field(args: &prost_types::Struct) -> Option<String> {
    let mut keys: Vec<_> = args.fields.keys().collect();
    keys.sort();
    keys.into_iter().find_map(|key| {
        find_template_in_value(&args.fields[key]).map(|rest| format!("{}{}", key, rest))
    })
}

fn find_template_in_value(value: &prost_types::Value) -> Option<String> {
    use prost_types::value::Kind;
    match value.kind.as_ref()? {
        Kind::StringValue(s) if s.contains("{{") || s.contains("}}") => Some(String::new()),
        Kind::StructValue(nested) => find_template_field(nested).map(|f| format!(".{}", f)),
        Kind::ListValue(list) => list
            .values
            .iter()
            .enumerate()
            .find_map(|(i, v)| find_template_in_value(v).map(|rest| format!("[{}]{}", i, rest))),
        _ => None,
    }
}

const MAX_GATE_TIMEOUT_SECONDS: i64 = 30 * 24 * 60 * 60;
const COMPLETION_SEARCH_BUDGET: u32 = 100_000;

struct Clause<'a> {
    step_id: &'a str,
    outcome: Option<&'a str>,
}

#[derive(Clone)]
struct OwnedClause {
    step_id: String,
    outcome: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TerminalKind {
    Open,
    Succeeded,
    Failed,
}

#[derive(Clone)]
struct OutcomeChoice {
    label: String,
    terminal: TerminalKind,
}

#[derive(Clone)]
enum StepDecision {
    Completed { label: String },
    Skipped,
}

#[derive(Clone)]
struct SearchNode {
    id: String,
    optional: bool,
    outcomes: Vec<OutcomeChoice>,
    after: Vec<OwnedClause>,
    after_any: Vec<OwnedClause>,
}

enum Availability {
    Ready,
    SkipContradicted,
    SkipBlocked,
}

enum ClauseSet {
    Satisfied,
    Contradicted,
    Blocked,
}

enum TemplateUse {
    Inputs,
    Step(String),
}

struct CompletionSearch {
    nodes: Vec<SearchNode>,
    decisions: HashMap<String, StepDecision>,
    completed: HashSet<String>,
    visits: u32,
    exhausted: bool,
    run_ok: bool,
}

fn validate_playbook_steps(spec: &PlaybookSpec, ids: &HashSet<&str>, errors: &mut ValidationError) {
    let mut labels: HashMap<String, HashSet<String>> = HashMap::new();
    let mut preds: HashMap<String, Vec<String>> = HashMap::new();
    for (index, step) in spec.steps.iter().enumerate() {
        let kind = validate_step_shape(errors, index, step);
        labels.insert(
            step.id.clone(),
            declared_outcome_labels(errors, index, step, kind),
        );
        preds.insert(step.id.clone(), predecessor_ids(step));
    }
    validate_step_references(errors, spec, ids, &labels);
    validate_step_templates(errors, spec, ids, &preds);
    validate_guards(errors, spec, ids, &labels);
    validate_playbook_graph(errors, spec, ids, &preds);
}

fn validate_step_shape(
    errors: &mut ValidationError,
    index: usize,
    step: &PlaybookStep,
) -> StepKind {
    let step_path = format!("steps[{index}]");
    if !step.id.is_empty() && !step_id_chars_ok(&step.id) {
        errors.push(
            &step_path,
            "step id must contain only letters, digits, '_' and '-'",
            "step_id",
        );
    }
    reject_retired(errors, &step_path, step);
    let kind = match effective_kind(step.kind) {
        Some(kind) => kind,
        None => {
            errors.push(
                format!("{step_path}.kind"),
                "step kind must be task or gate",
                "step_kind",
            );
            StepKind::Task
        }
    };
    match kind {
        StepKind::Gate => validate_gate_shape(errors, &step_path, step),
        _ => validate_task_shape(errors, &step_path, step),
    }
    if let Some(evidence) = &step.evidence {
        if evidence.tool.is_empty() {
            errors.push(
                format!("{step_path}.evidence.tool"),
                "evidence.tool is required",
                "evidence",
            );
        }
    }
    kind
}

fn reject_retired(errors: &mut ValidationError, step_path: &str, step: &PlaybookStep) {
    if step.retry_policy.is_some() {
        errors.push(
            format!("{step_path}.retry_policy"),
            "retry_policy is retired and rejected at publish",
            "retired",
        );
    }
    if !step.fallback_step_id.is_empty() {
        errors.push(
            format!("{step_path}.fallback_step_id"),
            "fallback_step_id is retired and rejected at publish",
            "retired",
        );
    }
    if step.join_policy != 0 {
        errors.push(
            format!("{step_path}.join_policy"),
            "join_policy is retired and rejected at publish",
            "retired",
        );
    }
    if step.required_successes != 0 {
        errors.push(
            format!("{step_path}.required_successes"),
            "required_successes is retired and rejected at publish",
            "retired",
        );
    }
    if step.evaluation_policy.is_some() {
        errors.push(
            format!("{step_path}.evaluation_policy"),
            "evaluation_policy is retired and rejected at publish",
            "retired",
        );
    }
}

fn validate_task_shape(errors: &mut ValidationError, step_path: &str, step: &PlaybookStep) {
    if step.instruction.is_empty() {
        errors.push(
            step_path,
            "Playbook step instruction is required",
            "required",
        );
    }
    if !step.question.is_empty() {
        errors.push(
            format!("{step_path}.question"),
            "question is only valid on gate steps",
            "task_field",
        );
    }
    if step.response_schema.is_some() {
        errors.push(
            format!("{step_path}.response_schema"),
            "response_schema is only valid on gate steps",
            "task_field",
        );
    }
    if !step.options.is_empty() {
        errors.push(
            format!("{step_path}.options"),
            "options are only valid on gate steps",
            "task_field",
        );
    }
    if !step.choices_from.is_empty() {
        errors.push(
            format!("{step_path}.choices_from"),
            "choices_from is only valid on gate steps",
            "task_field",
        );
    }
    if step.timeout.is_some() {
        errors.push(
            format!("{step_path}.timeout"),
            "timeout is only valid on gate steps",
            "task_field",
        );
    }
    if !step.on_timeout.is_empty() {
        errors.push(
            format!("{step_path}.on_timeout"),
            "on_timeout is only valid on gate steps",
            "task_field",
        );
    }
    if step.keep_warm.is_some() {
        errors.push(
            format!("{step_path}.keep_warm"),
            "keep_warm is only valid on gate steps",
            "task_field",
        );
    }
    validate_task_mode(errors, step_path, step);
}

fn validate_task_mode(errors: &mut ValidationError, step_path: &str, step: &PlaybookStep) {
    let mode = StepMode::try_from(step.mode).unwrap_or(StepMode::Unspecified);
    match mode {
        StepMode::Bound | StepMode::Assisted => match step.binding.as_ref() {
            Some(binding) if !binding.tool_name.is_empty() => {
                if mode == StepMode::Bound {
                    // Templates are allowed (G8); the host resolves them at run time.
                    if crate::canonical_args_digest(binding.arguments.as_ref()).is_err() {
                        errors.push(
                            format!("{step_path}.binding.arguments"),
                            "bound step arguments cannot be canonicalized",
                            "canonical_args",
                        );
                    }
                }
            }
            Some(_) => {
                errors.push(
                    step_path,
                    "bound and assisted steps require binding.tool_name",
                    "required",
                );
            }
            None => {
                errors.push(
                    step_path,
                    "bound and assisted steps require a tool binding",
                    "required",
                );
            }
        },
        StepMode::Agentic => {}
        StepMode::Unspecified => {
            errors.push(
                step_path,
                "Playbook step mode must be bound, assisted, or agentic",
                "required",
            );
        }
    }
}

fn validate_gate_shape(errors: &mut ValidationError, step_path: &str, step: &PlaybookStep) {
    if step.binding.is_some()
        || matches!(
            StepMode::try_from(step.mode),
            Ok(StepMode::Bound | StepMode::Assisted)
        )
    {
        errors.push(
            step_path,
            "gates cannot name a tool binding",
            "gate_binding",
        );
    }
    if !step.options.is_empty() && step.options.len() < 2 {
        errors.push(
            format!("{step_path}.options"),
            "gate options must contain at least two entries",
            "gate_options",
        );
    } else if step.options.len() < 2 && step.response_schema.is_none() {
        errors.push(
            step_path,
            "a gate needs at least two options or a response_schema",
            "gate_options",
        );
    }
    if let Some(timeout) = &step.timeout {
        if !duration_at_most(timeout, MAX_GATE_TIMEOUT_SECONDS) {
            errors.push(
                format!("{step_path}.timeout"),
                "gate timeout must be between 0 and 30 days",
                "gate_timeout",
            );
        }
    }
    if let Some(keep_warm) = &step.keep_warm {
        if !duration_non_negative(keep_warm) {
            errors.push(
                format!("{step_path}.keep_warm"),
                "keep_warm must not be negative",
                "keep_warm",
            );
        }
    }
}

fn validate_step_references(
    errors: &mut ValidationError,
    spec: &PlaybookSpec,
    ids: &HashSet<&str>,
    labels: &HashMap<String, HashSet<String>>,
) {
    for (index, step) in spec.steps.iter().enumerate() {
        let step_path = format!("steps[{index}]");
        for (field, tokens) in [
            ("after", &step.after),
            ("depends_on", &step.depends_on),
            ("after_any", &step.after_any),
        ] {
            for (token_index, token) in tokens.iter().enumerate() {
                check_precondition_token(
                    errors,
                    &format!("{step_path}.{field}[{token_index}]"),
                    token,
                    Some(step.id.as_str()),
                    ids,
                    labels,
                );
            }
        }
        if effective_kind(step.kind) == Some(StepKind::Gate) && !step.on_timeout.is_empty() {
            let declared = labels
                .get(&step.id)
                .is_some_and(|set| set.contains(&step.on_timeout));
            if !declared {
                errors.push(
                    format!("{step_path}.on_timeout"),
                    "on_timeout must name a declared outcome of this gate",
                    "on_timeout",
                );
            }
        }
    }
}

fn validate_step_templates(
    errors: &mut ValidationError,
    spec: &PlaybookSpec,
    ids: &HashSet<&str>,
    preds: &HashMap<String, Vec<String>>,
) {
    for (index, step) in spec.steps.iter().enumerate() {
        let step_path = format!("steps[{index}]");
        let ancestors = ancestor_ids(&step.id, preds);
        report_templates(
            errors,
            &format!("{step_path}.instruction"),
            &step.instruction,
            ids,
            &ancestors,
        );
        report_templates(
            errors,
            &format!("{step_path}.question"),
            &step.question,
            ids,
            &ancestors,
        );
        if let Some(arguments) = step
            .binding
            .as_ref()
            .and_then(|binding| binding.arguments.as_ref())
        {
            let mut keys: Vec<_> = arguments.fields.keys().cloned().collect();
            keys.sort();
            for key in keys {
                if let Some(value) = arguments.fields.get(&key) {
                    let mut texts = Vec::new();
                    collect_strings(
                        value,
                        &format!("{step_path}.binding.arguments.{key}"),
                        &mut texts,
                    );
                    for (path, text) in texts {
                        report_templates(errors, &path, &text, ids, &ancestors);
                    }
                }
            }
        }
        if let Some(schema) = step.response_schema.as_ref() {
            let mut texts = Vec::new();
            for (key, value) in &schema.fields {
                collect_strings(
                    value,
                    &format!("{step_path}.response_schema.{key}"),
                    &mut texts,
                );
            }
            texts.sort();
            for (path, text) in texts {
                if text.contains("{{") || text.contains("}}") {
                    report_templates(errors, &path, &text, ids, &ancestors);
                }
            }
        }
        if effective_kind(step.kind) == Some(StepKind::Gate) && !step.choices_from.is_empty() {
            match parse_output_ref(&step.choices_from) {
                Ok(id) if ancestors.contains(&id) => {}
                Ok(id) if !ids.contains(id.as_str()) => errors.push(
                    format!("{step_path}.choices_from"),
                    format!("choices_from references unknown step '{id}'"),
                    "choices_from",
                ),
                Ok(id) => errors.push(
                    format!("{step_path}.choices_from"),
                    format!("choices_from step '{id}' must precede this gate"),
                    "choices_from",
                ),
                Err(()) => errors.push(
                    format!("{step_path}.choices_from"),
                    "choices_from must be steps.<id>.output.<path>",
                    "choices_from",
                ),
            }
        }
    }
}

fn validate_guards(
    errors: &mut ValidationError,
    spec: &PlaybookSpec,
    ids: &HashSet<&str>,
    labels: &HashMap<String, HashSet<String>>,
) {
    for (index, guard) in spec.guards.iter().enumerate() {
        let path = format!("guards[{index}]");
        if guard.tools.is_empty() && guard.side_effects.is_empty() {
            errors.push(&path, "a guard needs tools or side_effects", "guard_target");
        }
        for (tool_index, tool) in guard.tools.iter().enumerate() {
            if tool.is_empty() {
                errors.push(
                    format!("{path}.tools[{tool_index}]"),
                    "guard tool name must not be empty",
                    "guard_target",
                );
            }
        }
        for (effect_index, effect) in guard.side_effects.iter().enumerate() {
            if !known_side_effect(*effect) {
                errors.push(
                    format!("{path}.side_effects[{effect_index}]"),
                    "guard side_effects must be a known side-effect class",
                    "guard_side_effect",
                );
            }
        }
        for (token_index, token) in guard.allowed_when.iter().enumerate() {
            check_precondition_token(
                errors,
                &format!("{path}.allowed_when[{token_index}]"),
                token,
                None,
                ids,
                labels,
            );
        }
        for (within_index, within) in guard.args_within.iter().enumerate() {
            let arg_path = format!("{path}.args_within[{within_index}]");
            if within.argument.is_empty() {
                errors.push(
                    format!("{arg_path}.argument"),
                    "args_within.argument is required",
                    "guard_reference",
                );
            }
            match parse_output_ref(&within.within) {
                Ok(id) if ids.contains(id.as_str()) => {}
                Ok(id) => errors.push(
                    format!("{arg_path}.within"),
                    format!("args_within references unknown step '{id}'"),
                    "guard_reference",
                ),
                Err(()) => errors.push(
                    format!("{arg_path}.within"),
                    "args_within.within must be steps.<id>.output.<path>",
                    "guard_reference",
                ),
            }
        }
    }
}

fn validate_playbook_graph(
    errors: &mut ValidationError,
    spec: &PlaybookSpec,
    ids: &HashSet<&str>,
    preds: &HashMap<String, Vec<String>>,
) {
    let known_ids: Vec<&str> = spec.steps.iter().map(|step| step.id.as_str()).collect();
    let mut edges: HashMap<&str, Vec<&str>> =
        known_ids.iter().map(|id| (*id, Vec::new())).collect();
    for step in &spec.steps {
        let Some(step_preds) = preds.get(&step.id) else {
            continue;
        };
        for pred in step_preds {
            if ids.contains(pred.as_str()) {
                if let Some(targets) = edges.get_mut(pred.as_str()) {
                    targets.push(step.id.as_str());
                }
            }
        }
    }
    let cyclic = cycle_nodes(&known_ids, &edges);
    if !cyclic.is_empty() {
        for step_id in &cyclic {
            errors.push(
                "steps",
                format!("Playbook contains a cycle involving step '{step_id}'"),
                "dag_no_cycles",
            );
        }
        return;
    }
    if known_ids.is_empty() {
        return;
    }
    let before = errors.field_violations.len();
    validate_reachability(errors, &known_ids, &edges);
    if errors.field_violations.len() == before {
        validate_completion(errors, spec);
    }
}

fn validate_completion(errors: &mut ValidationError, spec: &PlaybookSpec) {
    let mut preds = HashMap::new();
    let mut nodes = Vec::new();
    for step in &spec.steps {
        let kind = effective_kind(step.kind).unwrap_or(StepKind::Task);
        let depended = failed_is_depended_on(&step.id, &spec.steps, &spec.guards);
        nodes.push(SearchNode {
            id: step.id.clone(),
            optional: step.optional,
            outcomes: outcome_choices(step, kind, depended),
            after: owned_clauses(step.after.iter().chain(step.depends_on.iter())),
            after_any: owned_clauses(step.after_any.iter()),
        });
        preds.insert(step.id.clone(), predecessor_ids(step));
    }
    let ids: Vec<String> = nodes.iter().map(|node| node.id.clone()).collect();
    let order = topological(&ids, &preds);
    if order.len() != nodes.len() {
        return;
    }
    let by_id: HashMap<String, SearchNode> = nodes
        .into_iter()
        .map(|node| (node.id.clone(), node))
        .collect();
    let ordered = order
        .iter()
        .filter_map(|id| by_id.get(id).cloned())
        .collect();
    let mut search = CompletionSearch {
        nodes: ordered,
        decisions: HashMap::new(),
        completed: HashSet::new(),
        visits: 0,
        exhausted: false,
        run_ok: false,
    };
    search.walk(0);
    if search.exhausted {
        errors.push(
            "steps",
            "playbook is too large to check that a completion path exists",
            "completion_budget",
        );
        return;
    }
    if !search.run_ok {
        errors.push(
            "steps",
            "no outcome assignment lets this playbook complete",
            "completion",
        );
        return;
    }
    for step in &spec.steps {
        if !step.optional && !search.completed.contains(&step.id) {
            errors.push(
                "steps",
                format!("required step '{}' can never complete", step.id),
                "completion",
            );
        }
    }
}

impl CompletionSearch {
    fn walk(&mut self, idx: usize) {
        if self.exhausted {
            return;
        }
        if self.visits >= COMPLETION_SEARCH_BUDGET {
            self.exhausted = true;
            return;
        }
        self.visits += 1;
        if idx >= self.nodes.len() {
            self.run_ok = true;
            return;
        }
        let id = self.nodes[idx].id.clone();
        let optional = self.nodes[idx].optional;
        match self.availability(idx) {
            Availability::Ready => {
                let outcomes = self.nodes[idx].outcomes.clone();
                for outcome in &outcomes {
                    if outcome.terminal == TerminalKind::Failed {
                        continue;
                    }
                    self.decisions.insert(
                        id.clone(),
                        StepDecision::Completed {
                            label: outcome.label.clone(),
                        },
                    );
                    self.completed.insert(id.clone());
                    if outcome.terminal == TerminalKind::Succeeded {
                        self.run_ok = true;
                    } else {
                        self.walk(idx + 1);
                    }
                    self.decisions.remove(&id);
                    if self.exhausted {
                        return;
                    }
                }
                if optional {
                    self.decisions.insert(id.clone(), StepDecision::Skipped);
                    self.walk(idx + 1);
                    self.decisions.remove(&id);
                }
            }
            Availability::SkipContradicted => {
                self.decisions.insert(id.clone(), StepDecision::Skipped);
                self.walk(idx + 1);
                self.decisions.remove(&id);
            }
            Availability::SkipBlocked => {
                if optional {
                    self.decisions.insert(id.clone(), StepDecision::Skipped);
                    self.walk(idx + 1);
                    self.decisions.remove(&id);
                }
            }
        }
    }

    fn availability(&self, idx: usize) -> Availability {
        let node = &self.nodes[idx];
        let after = combine_and(&node.after, &self.decisions);
        let any = combine_any(&node.after_any, &self.decisions);
        match (after, any) {
            (ClauseSet::Satisfied, ClauseSet::Satisfied) => Availability::Ready,
            (ClauseSet::Blocked, _) | (_, ClauseSet::Blocked) => Availability::SkipBlocked,
            _ => Availability::SkipContradicted,
        }
    }
}

fn combine_and(clauses: &[OwnedClause], decisions: &HashMap<String, StepDecision>) -> ClauseSet {
    let mut contradicted = false;
    for clause in clauses {
        match clause_state(clause, decisions) {
            ClauseSet::Satisfied => {}
            ClauseSet::Contradicted => contradicted = true,
            ClauseSet::Blocked => return ClauseSet::Blocked,
        }
    }
    if contradicted {
        ClauseSet::Contradicted
    } else {
        ClauseSet::Satisfied
    }
}

fn combine_any(clauses: &[OwnedClause], decisions: &HashMap<String, StepDecision>) -> ClauseSet {
    if clauses.is_empty() {
        return ClauseSet::Satisfied;
    }
    let mut contradicted = false;
    let mut blocked = false;
    for clause in clauses {
        match clause_state(clause, decisions) {
            ClauseSet::Satisfied => return ClauseSet::Satisfied,
            ClauseSet::Contradicted => contradicted = true,
            ClauseSet::Blocked => blocked = true,
        }
    }
    if blocked {
        ClauseSet::Blocked
    } else if contradicted {
        ClauseSet::Contradicted
    } else {
        ClauseSet::Blocked
    }
}

fn clause_state(clause: &OwnedClause, decisions: &HashMap<String, StepDecision>) -> ClauseSet {
    match decisions.get(&clause.step_id) {
        Some(StepDecision::Completed { label }) => match clause.outcome.as_deref() {
            None => ClauseSet::Satisfied,
            Some(expected) if expected == label => ClauseSet::Satisfied,
            Some(_) => ClauseSet::Contradicted,
        },
        Some(StepDecision::Skipped) | None => ClauseSet::Blocked,
    }
}

fn check_precondition_token(
    errors: &mut ValidationError,
    path: &str,
    token: &str,
    owner: Option<&str>,
    ids: &HashSet<&str>,
    labels: &HashMap<String, HashSet<String>>,
) {
    let Some(clause) = parse_clause(token) else {
        errors.push(
            path,
            "precondition must be a step id or id:outcome",
            "valid_precondition",
        );
        return;
    };
    if owner == Some(clause.step_id) {
        errors.push(
            path,
            format!("step '{}' cannot depend on itself", clause.step_id),
            "no_self_dependency",
        );
        return;
    }
    if !ids.contains(clause.step_id) {
        errors.push(
            path,
            format!("precondition references unknown step '{}'", clause.step_id),
            "valid_precondition",
        );
        return;
    }
    if let Some(outcome) = clause.outcome {
        let declared = labels
            .get(clause.step_id)
            .is_some_and(|set| set.contains(outcome));
        if !declared {
            errors.push(
                path,
                format!(
                    "outcome '{outcome}' is not declared on step '{}'",
                    clause.step_id
                ),
                "declared_outcome",
            );
        }
    }
}

fn declared_outcome_labels(
    errors: &mut ValidationError,
    index: usize,
    step: &PlaybookStep,
    kind: StepKind,
) -> HashSet<String> {
    let step_path = format!("steps[{index}]");
    for (field, outcomes) in [("outcomes", &step.outcomes), ("options", &step.options)] {
        for (outcome_index, outcome) in outcomes.iter().enumerate() {
            let path = format!("{step_path}.{field}[{outcome_index}]");
            if OutcomeTerminal::try_from(outcome.terminal).is_err() {
                errors.push(
                    &path,
                    "outcome terminal must be succeeded or failed",
                    "outcome_terminal",
                );
            }
            if outcome.label.is_empty() || !step_id_chars_ok(&outcome.label) {
                errors.push(
                    &path,
                    "outcome label must contain only letters, digits, '_' and '-'",
                    "outcome_label",
                );
            }
        }
    }
    let mut seen_terminals: HashMap<&str, i32> = HashMap::new();
    for outcome in step.outcomes.iter().chain(step.options.iter()) {
        if outcome.label.is_empty() {
            continue;
        }
        if let Some(previous) = seen_terminals.get(outcome.label.as_str()) {
            if *previous != outcome.terminal {
                errors.push(
                    &step_path,
                    format!("outcome '{}' declares conflicting terminals", outcome.label),
                    "outcome_terminal",
                );
            }
        } else {
            seen_terminals.insert(outcome.label.as_str(), outcome.terminal);
        }
    }
    outcome_choices(step, kind, true)
        .into_iter()
        .map(|choice| choice.label)
        .collect()
}

fn outcome_choices(
    step: &PlaybookStep,
    kind: StepKind,
    failed_depended_on: bool,
) -> Vec<OutcomeChoice> {
    let author_failed = step
        .outcomes
        .iter()
        .chain(step.options.iter())
        .any(|outcome| outcome.label == "failed");
    let mut choices = Vec::new();
    if step.outcomes.is_empty() && step.options.is_empty() {
        if kind == StepKind::Task {
            push_choice(
                &mut choices,
                "done",
                map_terminal(
                    "done",
                    OutcomeTerminal::Unspecified as i32,
                    author_failed,
                    failed_depended_on,
                ),
            );
        }
        if kind == StepKind::Gate && step.response_schema.is_some() {
            for label in ["submitted", "rejected"] {
                push_choice(
                    &mut choices,
                    label,
                    map_terminal(
                        label,
                        OutcomeTerminal::Unspecified as i32,
                        author_failed,
                        failed_depended_on,
                    ),
                );
            }
        }
    } else {
        for outcome in step.outcomes.iter().chain(step.options.iter()) {
            push_choice(
                &mut choices,
                &outcome.label,
                map_terminal(
                    &outcome.label,
                    outcome.terminal,
                    author_failed,
                    failed_depended_on,
                ),
            );
        }
    }
    if kind == StepKind::Task && !choices.iter().any(|choice| choice.label == "failed") {
        push_choice(&mut choices, "failed", TerminalKind::Failed);
    }
    choices
}

fn push_choice(choices: &mut Vec<OutcomeChoice>, label: &str, terminal: TerminalKind) {
    if label.is_empty() || choices.iter().any(|choice| choice.label == label) {
        return;
    }
    choices.push(OutcomeChoice {
        label: label.to_string(),
        terminal,
    });
}

fn map_terminal(
    label: &str,
    terminal: i32,
    author_declared_failed: bool,
    failed_depended_on: bool,
) -> TerminalKind {
    match OutcomeTerminal::try_from(terminal) {
        Ok(OutcomeTerminal::Succeeded) => TerminalKind::Succeeded,
        Ok(OutcomeTerminal::Failed) => TerminalKind::Failed,
        _ if label == "failed" && !(author_declared_failed && failed_depended_on) => {
            TerminalKind::Failed
        }
        _ => TerminalKind::Open,
    }
}

fn failed_is_depended_on(step_id: &str, steps: &[PlaybookStep], guards: &[PlaybookGuard]) -> bool {
    let mentions = |token: &str| match parse_clause(token) {
        Some(clause) => {
            clause.step_id == step_id
                && (clause.outcome.is_none() || clause.outcome == Some("failed"))
        }
        None => false,
    };
    let steps_depend = steps.iter().any(|step| {
        step.id != step_id
            && step
                .after
                .iter()
                .chain(step.depends_on.iter())
                .chain(step.after_any.iter())
                .any(|token| mentions(token))
    });
    let guards_depend = guards
        .iter()
        .any(|guard| guard.allowed_when.iter().any(|token| mentions(token)));
    steps_depend || guards_depend
}

fn predecessor_ids(step: &PlaybookStep) -> Vec<String> {
    let mut preds = Vec::new();
    for token in step
        .after
        .iter()
        .chain(step.depends_on.iter())
        .chain(step.after_any.iter())
    {
        if let Some(clause) = parse_clause(token) {
            if !preds.iter().any(|id| id == clause.step_id) {
                preds.push(clause.step_id.to_string());
            }
        }
    }
    preds
}

fn owned_clauses<'a>(tokens: impl Iterator<Item = &'a String>) -> Vec<OwnedClause> {
    tokens
        .filter_map(|token| {
            parse_clause(token).map(|clause| OwnedClause {
                step_id: clause.step_id.to_string(),
                outcome: clause.outcome.map(str::to_string),
            })
        })
        .collect()
}

fn ancestor_ids(id: &str, preds: &HashMap<String, Vec<String>>) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut stack = preds.get(id).cloned().unwrap_or_default();
    while let Some(next) = stack.pop() {
        if out.insert(next.clone()) {
            if let Some(parents) = preds.get(&next) {
                stack.extend(parents.iter().cloned());
            }
        }
    }
    out
}

fn topological(ids: &[String], preds: &HashMap<String, Vec<String>>) -> Vec<String> {
    let index: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(position, id)| (id.as_str(), position))
        .collect();
    let mut indeg: HashMap<&str, usize> = ids.iter().map(|id| (id.as_str(), 0)).collect();
    let mut succs: HashMap<&str, Vec<&str>> =
        ids.iter().map(|id| (id.as_str(), Vec::new())).collect();
    for id in ids {
        if let Some(parents) = preds.get(id) {
            for pred in parents {
                if !indeg.contains_key(pred.as_str()) {
                    continue;
                }
                if let Some(count) = indeg.get_mut(id.as_str()) {
                    *count += 1;
                }
                if let Some(list) = succs.get_mut(pred.as_str()) {
                    list.push(id.as_str());
                }
            }
        }
    }
    let mut ready: Vec<&str> = indeg
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| *id)
        .collect();
    ready.sort_by_key(|id| index.get(id).copied().unwrap_or(0));
    let mut out = Vec::new();
    while !ready.is_empty() {
        let id = ready.remove(0);
        out.push(id.to_string());
        let next = succs.get(id).cloned().unwrap_or_default();
        for succ in next {
            if let Some(count) = indeg.get_mut(succ) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    let succ_index = index.get(succ).copied().unwrap_or(0);
                    let pos = ready
                        .partition_point(|item| index.get(item).copied().unwrap_or(0) < succ_index);
                    ready.insert(pos, succ);
                }
            }
        }
    }
    out
}

fn parse_clause(token: &str) -> Option<Clause<'_>> {
    if token.is_empty() {
        return None;
    }
    if let Some((step_id, outcome)) = token.split_once(':') {
        if step_id.is_empty() || outcome.is_empty() || outcome.contains(':') {
            return None;
        }
        return Some(Clause {
            step_id,
            outcome: Some(outcome),
        });
    }
    Some(Clause {
        step_id: token,
        outcome: None,
    })
}

fn step_id_chars_ok(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn effective_kind(kind: i32) -> Option<StepKind> {
    match StepKind::try_from(kind) {
        Ok(StepKind::Gate) => Some(StepKind::Gate),
        Ok(StepKind::Task) | Ok(StepKind::Unspecified) => Some(StepKind::Task),
        Err(_) => None,
    }
}

fn known_side_effect(value: i32) -> bool {
    matches!(
        SideEffectClass::try_from(value),
        Ok(SideEffectClass::ReadOnly)
            | Ok(SideEffectClass::MutableLocal)
            | Ok(SideEffectClass::MutableExternal)
    )
}

fn duration_non_negative(duration: &prost_types::Duration) -> bool {
    duration.seconds >= 0 && (0..1_000_000_000).contains(&duration.nanos)
}

fn duration_at_most(duration: &prost_types::Duration, max_seconds: i64) -> bool {
    duration_non_negative(duration)
        && (duration.seconds < max_seconds
            || (duration.seconds == max_seconds && duration.nanos == 0))
}

fn report_templates(
    errors: &mut ValidationError,
    path: &str,
    text: &str,
    ids: &HashSet<&str>,
    ancestors: &HashSet<String>,
) {
    let uses = match scan_templates(text) {
        Ok(uses) => uses,
        Err(()) => {
            errors.push(
                path,
                "template must be {{inputs.<path>}} or {{steps.<id>.output.<path>}}",
                "template_syntax",
            );
            return;
        }
    };
    for template in uses {
        let TemplateUse::Step(id) = template else {
            continue;
        };
        if !ids.contains(id.as_str()) {
            errors.push(
                path,
                format!("template references unknown step '{id}'"),
                "template_step",
            );
        } else if !ancestors.contains(&id) {
            errors.push(
                path,
                format!("template references step '{id}', which does not precede this step"),
                "template_precedence",
            );
        }
    }
}

fn scan_templates(text: &str) -> Result<Vec<TemplateUse>, ()> {
    let mut rest = text;
    let mut found = Vec::new();
    loop {
        if let Some(start) = rest.find("{{") {
            if rest[..start].contains("}}") {
                return Err(());
            }
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                return Err(());
            };
            let body = after[..end].trim();
            if body.is_empty() {
                return Err(());
            }
            found.push(parse_template_body(body)?);
            rest = &after[end + 2..];
        } else if rest.contains("}}") {
            return Err(());
        } else {
            return Ok(found);
        }
    }
}

fn parse_template_body(body: &str) -> Result<TemplateUse, ()> {
    if let Some(suffix) = body.strip_prefix("inputs") {
        if suffix.is_empty() || !is_output_suffix(suffix) {
            return Err(());
        }
        return Ok(TemplateUse::Inputs);
    }
    let Some(rest) = body.strip_prefix("steps.") else {
        return Err(());
    };
    let Some(output_at) = rest.find(".output") else {
        return Err(());
    };
    let id = &rest[..output_at];
    if !step_id_chars_ok(id) {
        return Err(());
    }
    let suffix = &rest[output_at + ".output".len()..];
    if !is_output_suffix(suffix) {
        return Err(());
    }
    Ok(TemplateUse::Step(id.to_string()))
}

fn parse_output_ref(raw: &str) -> Result<String, ()> {
    let trimmed = raw.trim();
    let body = if let Some(inner) = trimmed.strip_prefix("{{") {
        let Some(inner) = inner.strip_suffix("}}") else {
            return Err(());
        };
        if inner.contains("{{") || inner.contains("}}") {
            return Err(());
        }
        inner.trim()
    } else if trimmed.contains("{{") || trimmed.contains("}}") {
        return Err(());
    } else {
        trimmed
    };
    match parse_template_body(body) {
        Ok(TemplateUse::Step(id)) => Ok(id),
        _ => Err(()),
    }
}

fn is_output_suffix(mut path: &str) -> bool {
    while !path.is_empty() {
        if let Some(rest) = path.strip_prefix("[*]") {
            path = rest;
            continue;
        }
        if let Some(rest) = path.strip_prefix('[') {
            let Some((digits, after)) = rest.split_once(']') else {
                return false;
            };
            if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
                return false;
            }
            path = after;
            continue;
        }
        if let Some(rest) = path.strip_prefix('.') {
            let end = rest
                .find(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
                .unwrap_or(rest.len());
            if end == 0 {
                return false;
            }
            path = &rest[end..];
            continue;
        }
        return false;
    }
    true
}

fn collect_strings(value: &prost_types::Value, path: &str, out: &mut Vec<(String, String)>) {
    use prost_types::value::Kind;
    match &value.kind {
        Some(Kind::StringValue(text)) => out.push((path.to_string(), text.clone())),
        Some(Kind::StructValue(nested)) => {
            let mut keys: Vec<_> = nested.fields.keys().cloned().collect();
            keys.sort();
            for key in keys {
                if let Some(child) = nested.fields.get(&key) {
                    collect_strings(child, &format!("{path}.{key}"), out);
                }
            }
        }
        Some(Kind::ListValue(list)) => {
            for (index, child) in list.values.iter().enumerate() {
                collect_strings(child, &format!("{path}[{index}]"), out);
            }
        }
        _ => {}
    }
}

fn cycle_nodes<'a>(nodes: &[&'a str], edges: &HashMap<&'a str, Vec<&'a str>>) -> HashSet<&'a str> {
    fn visit<'a>(
        node: &'a str,
        edges: &HashMap<&'a str, Vec<&'a str>>,
        visiting: &mut HashSet<&'a str>,
        visited: &mut HashSet<&'a str>,
        cyclic: &mut HashSet<&'a str>,
    ) {
        if visited.contains(node) {
            return;
        }
        if !visiting.insert(node) {
            cyclic.insert(node);
            return;
        }
        if let Some(targets) = edges.get(node) {
            for target in targets {
                if visiting.contains(target) {
                    cyclic.insert(*target);
                    cyclic.insert(node);
                } else {
                    visit(target, edges, visiting, visited, cyclic);
                }
            }
        }
        visiting.remove(node);
        visited.insert(node);
    }

    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    let mut cyclic = HashSet::new();
    for node in nodes {
        visit(node, edges, &mut visiting, &mut visited, &mut cyclic);
    }
    cyclic
}

fn validate_reachability(
    errors: &mut ValidationError,
    nodes: &[&str],
    edges: &HashMap<&str, Vec<&str>>,
) {
    let mut incoming: HashMap<&str, usize> = nodes.iter().map(|node| (*node, 0)).collect();
    for targets in edges.values() {
        for target in targets {
            if let Some(count) = incoming.get_mut(target) {
                *count += 1;
            }
        }
    }
    let entries: Vec<_> = incoming
        .iter()
        .filter_map(|(node, count)| (*count == 0).then_some(*node))
        .collect();
    let mut reachable = HashSet::new();
    let mut stack = entries;
    while let Some(node) = stack.pop() {
        if reachable.insert(node) {
            if let Some(targets) = edges.get(node) {
                stack.extend(targets.iter().copied());
            }
        }
    }
    for node in nodes {
        if !reachable.contains(node) {
            errors.push(
                "steps",
                format!("Step '{}' is unreachable from every entry step", node),
                "reachable",
            );
        }
    }

    let terminals: HashSet<_> = nodes
        .iter()
        .copied()
        .filter(|node| edges.get(node).is_none_or(Vec::is_empty))
        .collect();
    let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
    for (source, targets) in edges {
        for target in targets {
            reverse.entry(target).or_default().push(source);
        }
    }
    let mut can_finish = HashSet::new();
    let mut stack: Vec<_> = terminals.iter().copied().collect();
    while let Some(node) = stack.pop() {
        if can_finish.insert(node) {
            if let Some(sources) = reverse.get(node) {
                stack.extend(sources.iter().copied());
            }
        }
    }
    for node in nodes {
        if !can_finish.contains(node) {
            errors.push(
                "steps",
                format!("Step '{}' cannot reach a terminal step", node),
                "terminal_reachability",
            );
        }
    }
}

impl Validate for Version {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();
        if self.major < 0 {
            errors.push("major", "Version major must be non-negative", "min_0");
        }
        if self.minor < 0 {
            errors.push("minor", "Version minor must be non-negative", "min_0");
        }
        if self.patch < 0 {
            errors.push("patch", "Version patch must be non-negative", "min_0");
        }
        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for Digest {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();
        if self.hex_hash.is_empty() {
            errors.push("hex_hash", "Digest hex_hash cannot be empty", "required");
        } else if !self
            .hex_hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        {
            errors.push(
                "hex_hash",
                format!(
                    "Digest hex_hash must be a lowercase hexadecimal string, got '{}'",
                    self.hex_hash
                ),
                "regex:^[a-f0-9]+$",
            );
        }
        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for Cost {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();
        if self.currency.len() != 3 || !self.currency.chars().all(|c| c.is_ascii_uppercase()) {
            errors.push(
                "currency",
                format!(
                    "Currency must be a 3-letter uppercase ISO 4217 code, got '{}'",
                    self.currency
                ),
                "pattern:^[A-Z]{3}$",
            );
        }
        if self.micro_amount < 0 {
            errors.push(
                "micro_amount",
                format!(
                    "Cost micro_amount must be non-negative, got {}",
                    self.micro_amount
                ),
                "min_0",
            );
        }
        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for TokenUsage {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();
        if self.prompt_tokens < 0 {
            errors.push(
                "prompt_tokens",
                "TokenUsage prompt_tokens must be non-negative",
                "min_0",
            );
        }
        if self.completion_tokens < 0 {
            errors.push(
                "completion_tokens",
                "TokenUsage completion_tokens must be non-negative",
                "min_0",
            );
        }
        if self.cached_tokens < 0 {
            errors.push(
                "cached_tokens",
                "TokenUsage cached_tokens must be non-negative",
                "min_0",
            );
        }
        if self.cache_creation_tokens < 0 {
            errors.push(
                "cache_creation_tokens",
                "TokenUsage cache_creation_tokens must be non-negative",
                "min_0",
            );
        }
        if self.total_tokens < 0 {
            errors.push(
                "total_tokens",
                "TokenUsage total_tokens must be non-negative",
                "min_0",
            );
        }
        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for ToolSpec {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();

        if !is_tool_name(&self.name) {
            errors.push(
                "name",
                format!("Tool name must be lowercase alphanumeric/underscores starting with a letter, got '{}'", self.name),
                "is_tool_name",
            );
        }

        if self.description.trim().is_empty() {
            errors.push(
                "description",
                "Tool description cannot be empty",
                "required",
            );
        }

        if self.input_schema_json.trim().is_empty() {
            errors.push(
                "input_schema_json",
                "Tool input_schema_json cannot be empty",
                "required",
            );
        }

        if self.execution_provider.is_none() {
            errors.push(
                "execution_provider",
                "Tool execution provider is required",
                "required",
            );
        } else if let Some(crate::v1::tool_spec::ExecutionProvider::LocalExec(local)) =
            &self.execution_provider
        {
            if let Err(err) = crate::validate_manifest_relative_entrypoint(&local.entrypoint) {
                errors.push(
                    "local_exec.entrypoint",
                    err.to_string(),
                    "manifest_relative_entrypoint",
                );
            }
        } else if let Some(crate::v1::tool_spec::ExecutionProvider::HostExec(host)) =
            &self.execution_provider
        {
            if host.adapter_id.trim().is_empty() {
                errors.push(
                    "host_exec.adapter_id",
                    "HostExecSpec adapter_id cannot be empty",
                    "required",
                );
            } else if !is_adapter_id(&host.adapter_id) {
                errors.push(
                    "host_exec.adapter_id",
                    format!(
                        "HostExecSpec adapter_id must be lowercase alphanumeric/underscores, got '{}'",
                        host.adapter_id
                    ),
                    "is_adapter_id",
                );
            }
            let secret_names: HashSet<&str> = self
                .required_secrets
                .iter()
                .map(|s| s.name.as_str())
                .collect();
            for (key, value) in &host.config {
                if secret_names.contains(key.as_str()) {
                    errors.push(
                        format!("host_exec.config.{}", key),
                        format!(
                            "HostExecSpec config key '{}' matches a required_secrets name; secrets must not appear in config",
                            key
                        ),
                        "no_secret_in_config",
                    );
                }
                if secret_names.contains(value.as_str()) {
                    errors.push(
                        format!("host_exec.config.{}", key),
                        format!(
                            "HostExecSpec config value for '{}' must not equal a required_secrets name",
                            key
                        ),
                        "no_secret_in_config",
                    );
                }
            }
        }

        crate::skill_egress::validate_tool_egress(self, &mut errors);
        validate_tool_ui(self, &mut errors);

        for (index, requirement) in self.required_secrets.iter().enumerate() {
            if requirement.name.trim().is_empty() {
                errors.push(
                    format!("required_secrets[{}].name", index),
                    "SecretRequirement name cannot be empty",
                    "required",
                );
            }
            if !(0..=3).contains(&requirement.scope) {
                errors.push(
                    format!("required_secrets[{}].scope", index),
                    format!(
                        "SecretRequirement scope must be 0-3 (unspecified/org/user/either), got {}",
                        requirement.scope
                    ),
                    "valid_scope",
                );
            }
        }

        if let Some(timeout) = &self.timeout {
            if timeout.seconds < 0 || timeout.nanos < 0 || timeout.nanos >= 1_000_000_000 {
                errors.push(
                    "timeout",
                    "Tool timeout duration is invalid",
                    "valid_duration",
                );
            }
        }

        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn is_ui_name(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn validate_tool_ui(tool: &ToolSpec, errors: &mut ValidationError) {
    let Some(ui) = &tool.ui else { return };
    for (field, value) in [("ui.show", &ui.show), ("ui.ask", &ui.ask)] {
        if !value.is_empty() && !is_ui_name(value) {
            errors.push(
                field,
                format!("ToolUi {} must be a card name (lowercase letters, digits, underscores; starts with a letter; at most 64), got '{}'", &field[3..], value),
                "is_ui_name",
            );
        }
    }
    if !ui.show.is_empty() && tool.output_schema_json.trim().is_empty() {
        errors.push(
            "ui.show",
            "ToolUi show needs a non-empty output_schema_json",
            "show_needs_output_schema",
        );
    }
}

impl Validate for SkillPackageRecord {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();

        if let Some(resource_id) = &self.resource_id {
            if let Err(mut err) = resource_id.validate() {
                err.prefix_path("resource_id");
                errors.merge(err);
            } else if !resource_id.urn.contains(":skill:") {
                errors.push(
                    "resource_id",
                    "SkillPackageRecord resource URN must be of kind 'skill'",
                    "skill_kind",
                );
            }
        } else {
            errors.push(
                "resource_id",
                "SkillPackageRecord resource_id is required",
                "required",
            );
        }

        if let Some(version) = &self.version {
            if let Err(mut err) = version.validate() {
                err.prefix_path("version");
                errors.merge(err);
            }
        } else {
            errors.push(
                "version",
                "SkillPackageRecord version is required",
                "required",
            );
        }

        if self.digest.is_empty() {
            errors.push(
                "digest",
                "SkillPackageRecord digest is required",
                "required",
            );
        } else if let Err(err) = (Digest {
            algorithm: 1,
            hex_hash: self.digest.clone(),
        })
        .validate()
        {
            errors.merge(err);
        }

        let channel = SkillChannel::try_from(self.channel).unwrap_or(SkillChannel::Unspecified);
        let prerelease = self
            .version
            .as_ref()
            .map(|version| version.prerelease.as_str())
            .unwrap_or("");
        match channel {
            SkillChannel::Draft => {
                if prerelease != "draft" {
                    errors.push(
                        "version.prerelease",
                        "DRAFT channel requires prerelease 'draft'",
                        "channel_prerelease_agreement",
                    );
                }
            }
            SkillChannel::Released => {
                if !prerelease.is_empty() {
                    errors.push(
                        "version.prerelease",
                        "RELEASED channel requires an empty prerelease",
                        "channel_prerelease_agreement",
                    );
                }
            }
            SkillChannel::Unspecified => {
                errors.push(
                    "channel",
                    "SkillPackageRecord channel is required",
                    "required",
                );
            }
        }

        if self.blob_relpath.trim().is_empty() {
            errors.push(
                "blob_relpath",
                "SkillPackageRecord blob_relpath is required",
                "required",
            );
        }

        if self.size_bytes < 0 {
            errors.push(
                "size_bytes",
                "SkillPackageRecord size_bytes must be non-negative",
                "min_0",
            );
        }

        if let Some(spec) = &self.spec {
            if let Err(mut err) = spec.validate() {
                err.prefix_path("spec");
                errors.merge(err);
            }
        } else {
            errors.push("spec", "SkillPackageRecord spec is required", "required");
        }

        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for SkillManifest {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();

        if let Some(resource_id) = &self.resource_id {
            if let Err(mut err) = resource_id.validate() {
                err.prefix_path("resource_id");
                errors.merge(err);
            } else if !resource_id.urn.contains(":skill:") {
                errors.push(
                    "resource_id",
                    "SkillManifest resource URN must be of kind 'skill'",
                    "skill_kind",
                );
            }
        } else {
            errors.push(
                "resource_id",
                "SkillManifest resource_id is required",
                "required",
            );
        }

        if let Some(version) = &self.version {
            if let Err(mut err) = version.validate() {
                err.prefix_path("version");
                errors.merge(err);
            }
        } else {
            errors.push("version", "SkillManifest version is required", "required");
        }

        crate::skill_egress::validate_manifest_egress(self, &mut errors);
        crate::runtime_requirement::validate_requires_runtime(&self.requires_runtime, &mut errors);

        let mut tool_names = HashSet::new();
        for (i, tool) in self.tools.iter().enumerate() {
            if let Err(mut err) = tool.validate() {
                err.prefix_path(&format!("tools[{}]", i));
                errors.merge(err);
            }
            validate_effect(
                &mut errors,
                &format!("tools[{}]", i),
                &tool.name,
                tool.side_effect,
                tool.effect.as_ref(),
                |name| {
                    self.tools
                        .iter()
                        .find(|t| t.name == name)
                        .map(|t| t.side_effect)
                },
            );
            if !tool_names.insert(tool.name.as_str()) {
                errors.push(
                    format!("tools[{}]", i),
                    format!(
                        "Duplicate tool name '{}' within the same skill manifest",
                        tool.name
                    ),
                    "unique_tool_names",
                );
            }
            if is_reserved_builtin_name(&tool.name) {
                errors.push(
                    format!("tools[{}].name", i),
                    format!(
                        "Tool name '{}' is reserved for runtime builtin primitives and cannot be declared in a skill manifest",
                        tool.name
                    ),
                    "reserved_builtin_name",
                );
            }
        }

        for (i, capability) in self.capabilities.iter().enumerate() {
            if let Err(reason) = validate_capability_token(capability) {
                errors.push(format!("capabilities[{}]", i), reason, "capability_shape");
            }
        }

        // `policy` is deprecated and ignored. Publish accepts it and returns
        // [`skill_manifest_warnings`]. Nothing enforces the Cedar. A skill marks
        // its own tools `approval_required` instead.

        let mut seen_playbooks = HashSet::new();
        for (i, path) in self.playbooks.iter().enumerate() {
            if let Err(err) = crate::skill_entrypoint::validate_manifest_relative_entrypoint(path) {
                errors.push(
                    format!("playbooks[{i}]"),
                    err.to_string(),
                    "manifest_relative",
                );
            }
            if !seen_playbooks.insert(path.as_str()) {
                errors.push(
                    format!("playbooks[{i}]"),
                    format!("duplicate playbook path '{path}'"),
                    "unique",
                );
            }
        }

        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for AgentSpec {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();

        if let Some(resource_id) = &self.resource_id {
            if let Err(mut err) = resource_id.validate() {
                err.prefix_path("resource_id");
                errors.merge(err);
            } else if !resource_id.urn.contains(":agent:") {
                errors.push(
                    "resource_id",
                    "AgentSpec resource URN must be of kind 'agent'",
                    "agent_kind",
                );
            }
        } else {
            errors.push(
                "resource_id",
                "AgentSpec resource_id is required",
                "required",
            );
        }

        if self.display_name.trim().is_empty() {
            errors.push(
                "display_name",
                "AgentSpec display_name cannot be empty",
                "required",
            );
        }

        if self.role.trim().is_empty() {
            errors.push("role", "AgentSpec role cannot be empty", "required");
        }

        if self.extends.is_some() && self.instruction_source.is_some() {
            errors.push(
                "instruction_source",
                "An extending agent states instruction changes on overlay.instructions, not instruction_source",
                "instruction_source_not_overlay",
            );
        }
        if self.extends.is_some() && !self.skill_refs.is_empty() {
            errors.push(
                "skill_refs",
                "An extending agent adds skills on overlay.skills.add, not skill_refs",
                "skill_refs_not_overlay",
            );
        }
        if self.extends.is_none() && self.overlay.is_some() {
            errors.push(
                "overlay",
                "AgentOverlay is only valid when extends is set",
                "overlay_without_extends",
            );
        }
        if self.extends.is_none() && self.instruction_source.is_none() {
            errors.push(
                "instruction_source",
                "AgentSpec instruction source is required",
                "required",
            );
        } else if let Some(source) = &self.instruction_source {
            match source {
                crate::v1::agent_spec::InstructionSource::SystemInstructions(sys) => {
                    if sys.trim().is_empty() {
                        errors.push(
                            "system_instructions",
                            "AgentSpec system_instructions cannot be empty",
                            "required",
                        );
                    }
                }
                crate::v1::agent_spec::InstructionSource::InstructionArtifact(artifact) => {
                    if let Err(mut err) = artifact.validate() {
                        err.prefix_path("instruction_artifact");
                        errors.merge(err);
                    }
                }
            }
        }

        for (i, skill_ref) in self.skill_refs.iter().enumerate() {
            if let Err(mut err) = validate_resolvable_ref(skill_ref) {
                err.prefix_path(&format!("skill_refs[{}]", i));
                errors.merge(err);
            }
        }

        // `extends` may float in a draft. Resolution pins it, so it takes the
        // authored rule, not the full pin that `ResourceRef::validate` requires.
        if let Some(extends) = &self.extends {
            if let Err(mut err) = validate_resolvable_ref(extends) {
                err.prefix_path("extends");
                errors.merge(err);
            }
            let urn = extends
                .resource_id
                .as_ref()
                .map(|id| id.urn.as_str())
                .unwrap_or("");
            if !urn.is_empty() && !urn.contains(":agent:") {
                errors.push("extends", "extends must be an agent URN", "agent_kind");
            }
            let self_urn = self
                .resource_id
                .as_ref()
                .map(|id| id.urn.as_str())
                .unwrap_or("");
            if !self_urn.is_empty() && urn == self_urn {
                errors.push("extends", "an agent cannot extend itself", "extends_cycle");
            }
        }

        if let Some(overlay) = &self.overlay {
            validate_agent_overlay(overlay, &mut errors);
        }

        // Host-governed policy refs (AX-3.1). Each one is optional. When present
        // it must name a policy.
        let policy_refs = [
            (
                "capability_policy.policy_ref",
                self.capability_policy
                    .as_ref()
                    .and_then(|policy| policy.policy_ref.as_ref()),
            ),
            (
                "hitl_policy.policy_ref",
                self.hitl_policy
                    .as_ref()
                    .and_then(|policy| policy.policy_ref.as_ref()),
            ),
            (
                "telemetry_policy.policy_ref",
                self.telemetry_policy
                    .as_ref()
                    .and_then(|policy| policy.policy_ref.as_ref()),
            ),
            (
                "completion_policy.policy_ref",
                self.completion_policy
                    .as_ref()
                    .and_then(|policy| policy.policy_ref.as_ref()),
            ),
        ];
        for (path, policy_ref) in policy_refs {
            if let Some(policy_ref) = policy_ref {
                validate_kinded_ref(policy_ref, path, "policy", &mut errors);
            }
        }

        // Evaluator refs share the authored rule (URN shape and digest rule).
        // No evaluator URN kind is defined yet, so their kind is not checked.
        if let Some(completion_policy) = &self.completion_policy {
            for (i, evaluator_ref) in completion_policy.evaluator_refs.iter().enumerate() {
                if let Err(mut err) = validate_resolvable_ref(evaluator_ref) {
                    err.prefix_path(&format!("completion_policy.evaluator_refs[{i}]"));
                    errors.merge(err);
                }
            }
        }

        if let Some(default_playbook_ref) = &self.default_playbook_ref {
            validate_kinded_ref(
                default_playbook_ref,
                "default_playbook_ref",
                "playbook",
                &mut errors,
            );
        }
        for (i, playbook_ref) in self.allowed_playbook_refs.iter().enumerate() {
            validate_kinded_ref(
                playbook_ref,
                &format!("allowed_playbook_refs[{i}]"),
                "playbook",
                &mut errors,
            );
        }

        if let Some(context_policy) = &self.context_policy {
            if context_policy.max_token_count < 0 {
                errors.push(
                    "context_policy.max_token_count",
                    "max_token_count must be non-negative",
                    "min_0",
                );
            }
            if context_policy.max_history_turns < 0 {
                errors.push(
                    "context_policy.max_history_turns",
                    "max_history_turns must be non-negative",
                    "min_0",
                );
            }
        }

        if let Some(reasoning_policy) = &self.reasoning_policy {
            // 0 means unset: create_run applies the default. A value above the
            // run hard cap stays valid here and is clamped when the run is stored.
            if reasoning_policy.max_iterations < 0 {
                errors.push(
                    "reasoning_policy.max_iterations",
                    "max_iterations must be non-negative",
                    "min_0",
                );
            }
            if let Some(step_timeout) = &reasoning_policy.step_timeout {
                if step_timeout.seconds < 0
                    || step_timeout.nanos < 0
                    || step_timeout.nanos >= 1_000_000_000
                {
                    errors.push(
                        "reasoning_policy.step_timeout",
                        "step_timeout must be non-negative",
                        "min_0",
                    );
                }
            }
        }

        // AX-5.1. Legacy specs keep `capability_policy` and `hitl_policy` and
        // do not set `permissions`. A spec that sets both is rejected: hand
        // written Cedar belongs on `permissions.advanced`.
        if let Some(permissions) = &self.permissions {
            if self.capability_policy.is_some() || self.hitl_policy.is_some() {
                errors.push(
                    "permissions",
                    concat!(
                        "permissions cannot be combined with capability_policy or hitl_policy; ",
                        "use `permissions.advanced` for hand-written Cedar"
                    ),
                    "permissions_not_legacy",
                );
            }
            if let Err(err) = crate::permissions::validate_agent_permissions(permissions) {
                errors.merge(err);
            }
            for (i, policy_ref) in permissions.advanced.iter().enumerate() {
                validate_kinded_ref(
                    policy_ref,
                    &format!("permissions.advanced[{i}]"),
                    "policy",
                    &mut errors,
                );
            }
        }

        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn validate_agent_overlay(overlay: &crate::v1::AgentOverlay, errors: &mut ValidationError) {
    if let Some(instructions) = &overlay.instructions {
        if let Some(change) = &instructions.change {
            let text = match change {
                crate::v1::instruction_overlay::Change::Append(text)
                | crate::v1::instruction_overlay::Change::Replace(text) => text,
            };
            if text.trim().is_empty() {
                errors.push(
                    "overlay.instructions",
                    "instruction overlay text cannot be empty",
                    "instruction_empty",
                );
            }
        }
    }

    if let Some(skills) = &overlay.skills {
        let mut added: HashMap<String, &ResourceRef> = HashMap::new();
        for (i, skill_ref) in skills.add.iter().enumerate() {
            let path = format!("overlay.skills.add[{i}]");
            validate_overlay_skill_ref(skill_ref, &path, errors);
            let urn = skill_ref
                .resource_id
                .as_ref()
                .map(|id| id.urn.as_str())
                .unwrap_or("");
            if urn.is_empty() {
                continue;
            }
            if let Some(previous) = added.get(urn) {
                // A floating ref and a pinned ref to one URN are not a conflict
                // at validation time. Resolution pins the floating one, and the
                // merge compares the pins (`extends::merge_agent_chain`).
                if previous.version.is_some() != skill_ref.version.is_some() {
                    continue;
                }
                let constraint = if crate::extends::same_resource_pin(previous, skill_ref) {
                    "unique"
                } else {
                    "skill_version_conflict"
                };
                errors.push(
                    &path,
                    format!("skill '{urn}' is added more than once"),
                    constraint,
                );
            } else {
                added.insert(urn.to_string(), skill_ref);
            }
        }
        let mut removed = HashSet::new();
        for (i, skill_ref) in skills.remove.iter().enumerate() {
            let path = format!("overlay.skills.remove[{i}]");
            validate_overlay_skill_ref(skill_ref, &path, errors);
            let urn = skill_ref
                .resource_id
                .as_ref()
                .map(|id| id.urn.as_str())
                .unwrap_or("");
            if !urn.is_empty() && !removed.insert(urn.to_string()) {
                errors.push(&path, format!("duplicate skill ref '{urn}'"), "unique");
            }
        }
    }

    if let Some(capabilities) = &overlay.capabilities {
        let mut granted = HashSet::new();
        for (i, token) in capabilities.grant.iter().enumerate() {
            if let Err(reason) = validate_capability_token(token) {
                errors.push(
                    format!("overlay.capabilities.grant[{i}]"),
                    reason,
                    "capability_shape",
                );
            }
            granted.insert(token.as_str());
        }
        for (i, token) in capabilities.revoke.iter().enumerate() {
            if let Err(reason) = validate_capability_token(token) {
                errors.push(
                    format!("overlay.capabilities.revoke[{i}]"),
                    reason,
                    "capability_shape",
                );
            }
            if granted.contains(token.as_str()) {
                errors.push(
                    format!("overlay.capabilities.revoke[{i}]"),
                    format!("capability '{token}' is both granted and revoked"),
                    "capability_grant_revoke_conflict",
                );
            }
        }
    }
}

fn validate_overlay_skill_ref(skill_ref: &ResourceRef, path: &str, errors: &mut ValidationError) {
    if let Err(mut err) = validate_resolvable_ref(skill_ref) {
        err.prefix_path(path);
        errors.merge(err);
    }
    let urn = skill_ref
        .resource_id
        .as_ref()
        .map(|id| id.urn.as_str())
        .unwrap_or("");
    if !urn.is_empty() && !urn.contains(":skill:") {
        errors.push(path, "overlay skill refs must be skill URNs", "skill_kind");
    }
}

impl Validate for RunSpec {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();

        if let Some(run_id) = &self.run_id {
            if let Err(mut err) = run_id.validate() {
                err.prefix_path("run_id");
                errors.merge(err);
            } else if !run_id.urn.contains(":run:") {
                errors.push(
                    "run_id",
                    "RunSpec resource URN must be of kind 'run'",
                    "run_kind",
                );
            }
        } else {
            errors.push("run_id", "RunSpec run_id is required", "required");
        }

        if let Some(agent_ref) = &self.agent_ref {
            if let Err(mut err) = agent_ref.validate() {
                err.prefix_path("agent_ref");
                errors.merge(err);
            }
        } else {
            errors.push("agent_ref", "RunSpec agent_ref is required", "required");
        }

        if let Some(playbook_ref) = &self.playbook_ref {
            // Not in AX-3.1's scope: a run keeps the full-pin rule once a version
            // is present.
            let checked = if playbook_ref.version.is_some() {
                playbook_ref.validate()
            } else {
                validate_resolvable_ref(playbook_ref)
            };
            if let Err(mut err) = checked {
                err.prefix_path("playbook_ref");
                errors.merge(err);
            }
        } else {
            errors.push(
                "playbook_ref",
                "RunSpec playbook_ref is required",
                "required",
            );
        }

        for (i, resolved) in self.resolved_refs.iter().enumerate() {
            if let Err(mut err) = resolved.validate() {
                err.prefix_path(&format!("resolved_refs[{}]", i));
                errors.merge(err);
            }
        }

        if let Some(max_duration) = &self.max_duration {
            if max_duration.seconds < 0
                || max_duration.nanos < 0
                || max_duration.nanos >= 1_000_000_000
            {
                errors.push(
                    "max_duration",
                    "RunSpec max_duration is invalid",
                    "valid_duration",
                );
            }
        }

        if let Some(budget) = &self.max_financial_budget {
            if let Err(mut err) = budget.validate() {
                err.prefix_path("max_financial_budget");
                errors.merge(err);
            }
        }

        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for ApprovalRequestClaims {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();
        if let Some(id) = &self.request_id {
            if let Err(mut err) = id.validate() {
                err.prefix_path("request_id");
                errors.merge(err);
            }
        } else {
            errors.push(
                "request_id",
                "ApprovalRequestClaims.request_id is required",
                "required",
            );
        }
        if let Some(digest) = &self.requested_args_digest {
            if let Err(mut err) = digest.validate() {
                err.prefix_path("requested_args_digest");
                errors.merge(err);
            }
        }
        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for ApprovalDecisionClaims {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();
        let decision = approval_decision_claims::HitlDecision::try_from(self.decision)
            .unwrap_or(approval_decision_claims::HitlDecision::Unspecified);
        if decision == approval_decision_claims::HitlDecision::Rejected
            && self.rejection_reason.trim().is_empty()
        {
            errors.push(
                "rejection_reason",
                "rejection_reason is required when decision is REJECTED",
                "required_for_reject",
            );
        }
        if decision == approval_decision_claims::HitlDecision::Amended
            && self.amended_args.is_none()
        {
            errors.push(
                "amended_args",
                "amended_args is required when decision is AMENDED",
                "required_for_amend",
            );
        }
        if let Some(digest) = &self.amended_args_digest {
            if let Err(mut err) = digest.validate() {
                err.prefix_path("amended_args_digest");
                errors.merge(err);
            }
        }
        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Validate for SignedEnvelope {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();

        if self.domain.trim().is_empty() {
            errors.push(
                "domain",
                "SignedEnvelope domain cannot be empty",
                "required",
            );
        }

        if let Some(version) = &self.envelope_version {
            if let Err(mut err) = version.validate() {
                err.prefix_path("envelope_version");
                errors.merge(err);
            }
        } else {
            errors.push(
                "envelope_version",
                "SignedEnvelope envelope_version is required",
                "required",
            );
        }

        if self.claims.is_empty() {
            errors.push(
                "claims",
                "SignedEnvelope claims cannot be empty",
                "required",
            );
        }

        if let Some(sig) = &self.signature {
            if sig.key_id.trim().is_empty() {
                errors.push(
                    "signature.key_id",
                    "Signature key_id cannot be empty",
                    "required",
                );
            }
            if sig.value.is_empty() {
                errors.push(
                    "signature.value",
                    "Signature value bytes cannot be empty",
                    "required",
                );
            }
        } else {
            errors.push(
                "signature",
                "SignedEnvelope signature is required",
                "required",
            );
        }

        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

pub(crate) fn is_slug(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

pub(crate) fn is_registry(value: &str) -> bool {
    !value.is_empty()
        && value
            .split('.')
            .all(|segment| is_slug(segment) && !segment.contains('_'))
}

/// Guest-visible tool names, including `{connection}__{vendor}` MCP ids.
/// 128 covers a connection prefix plus a long vendor tool name.
pub const TOOL_NAME_MAX_LEN: usize = 128;

pub fn is_tool_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= TOOL_NAME_MAX_LEN
        && value
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Reserved tool names owned by the runtime builtin primitive set (M1).
pub fn reserved_builtin_tool_names() -> &'static [&'static str] {
    &[
        "read_file",
        "write_file",
        "edit_file",
        "delete_file",
        "glob",
        "grep",
        "run",
        "todo_write",
        "load_skill",
    ]
}

pub fn is_reserved_builtin_name(name: &str) -> bool {
    reserved_builtin_tool_names().contains(&name)
}

/// Structured capability token: `tool:`, `egress:`, `secret:`, or `fs:` plus a
/// non-empty remainder. `fs:workspace:write` is valid. This is shape only.
pub fn validate_capability_token(token: &str) -> Result<(), String> {
    if token.chars().any(char::is_whitespace) {
        return Err(format!("capability '{token}' must not contain whitespace"));
    }
    let Some((kind, rest)) = token.split_once(':') else {
        return Err(format!(
            "capability '{token}' must start with tool:, egress:, secret:, or fs:"
        ));
    };
    if !matches!(kind, "tool" | "egress" | "secret" | "fs") || rest.is_empty() {
        return Err(format!(
            "capability '{token}' must start with tool:, egress:, secret:, or fs: and have a non-empty remainder"
        ));
    }
    Ok(())
}

/// SHA-256 digest for a runtime builtin tool (`sha256("vam-builtin-v1:<name>")`).
pub fn builtin_tool_digest(name: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("vam-builtin-v1:{}", name).as_bytes())
    )
}

/// SHA-256 digest for a host adapter tool (`sha256("vam-host-v1:<adapter_id>")`).
pub fn host_tool_digest(adapter_id: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("vam-host-v1:{}", adapter_id).as_bytes())
    )
}

pub fn validate_trajectory_digest(digest: &str) -> Result<(), ValidationError> {
    if digest.is_empty() {
        return Err(ValidationError::new(
            "trajectory_digest",
            "trajectory_digest cannot be empty when present",
            "required",
        ));
    }
    Digest {
        algorithm: 1,
        hex_hash: digest.to_string(),
    }
    .validate()
}

pub fn validate_thread_turn_trajectory(
    trajectory_digest: Option<&str>,
    touched_paths: &[String],
    skill_ids: &[String],
    tool_names: &[String],
) -> Result<(), ValidationError> {
    let mut errors = ValidationError::empty();
    if let Some(digest) = trajectory_digest {
        if let Err(err) = validate_trajectory_digest(digest) {
            errors.merge(err);
        }
    }
    if touched_paths.len() > 32 {
        errors.push(
            "touched_paths",
            format!(
                "touched_paths must contain at most 32 entries, got {}",
                touched_paths.len()
            ),
            "max_items_32",
        );
    }
    for (i, path) in touched_paths.iter().enumerate() {
        if path.contains("..") || path.starts_with('/') {
            errors.push(
                format!("touched_paths[{}]", i),
                format!("touched_paths entry must be workspace-relative: '{}'", path),
                "relative_path",
            );
        }
    }
    for (i, skill) in skill_ids.iter().enumerate() {
        if skill.trim().is_empty() {
            errors.push(
                format!("skill_ids[{}]", i),
                "skill_ids entries must not be empty",
                "non_empty",
            );
        }
    }
    for (i, tool) in tool_names.iter().enumerate() {
        if !is_tool_name(tool) {
            errors.push(
                format!("tool_names[{}]", i),
                format!("tool_names entry is invalid: '{}'", tool),
                "is_tool_name",
            );
        }
    }
    if errors.field_violations.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

impl Validate for crate::v1::ThreadTurnPayload {
    fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = ValidationError::empty();
        if self.prompt.trim().is_empty() {
            errors.push(
                "prompt",
                "ThreadTurnPayload.prompt cannot be empty",
                "required",
            );
        }
        if self.answer.trim().is_empty() {
            errors.push(
                "answer",
                "ThreadTurnPayload.answer cannot be empty",
                "required",
            );
        }
        if self.model.trim().is_empty() {
            errors.push(
                "model",
                "ThreadTurnPayload.model cannot be empty",
                "required",
            );
        }
        if self.manifest_hash.trim().is_empty() {
            errors.push(
                "manifest_hash",
                "ThreadTurnPayload.manifest_hash cannot be empty",
                "required",
            );
        }
        if self.prev_hash.trim().is_empty() {
            errors.push(
                "prev_hash",
                "ThreadTurnPayload.prev_hash cannot be empty",
                "required",
            );
        }
        if let Err(err) = validate_thread_turn_trajectory(
            self.trajectory_digest.as_deref(),
            &self.touched_paths,
            &self.skill_ids,
            &self.tool_names,
        ) {
            errors.merge(err);
        }
        if errors.field_violations.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Maximum length for a hosted adapter id (`hosted:<broker_id>:<tool_name>`).
const HOSTED_ADAPTER_ID_MAX_LEN: usize = 512;

fn is_adapter_id(value: &str) -> bool {
    if parse_hosted_adapter_id(value).is_some() || parse_mcp_adapter_id(value).is_some() {
        return value.len() <= HOSTED_ADAPTER_ID_MAX_LEN;
    }
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Parse `hosted:<broker_id>:<tool_name>` into its components.
pub fn parse_hosted_adapter_id(id: &str) -> Option<(String, String)> {
    let rest = id.strip_prefix("hosted:")?;
    let (broker_id, tool_name) = rest.split_once(':')?;
    if !is_broker_id(broker_id) || tool_name.is_empty() {
        return None;
    }
    Some((broker_id.to_string(), tool_name.to_string()))
}

/// Build the vamd-assigned adapter id for an HSP-exported tool.
pub fn hosted_adapter_id(broker_id: &str, tool_name: &str) -> String {
    format!("hosted:{broker_id}:{tool_name}")
}

/// Parse `mcp:<connection_id>:<tool_name>`.
///
/// `connection_id` uses the registry charset (`[a-z0-9_-]`, 1–64). The tool
/// segment may contain further colons. Same length cap as a hosted id.
pub fn parse_mcp_adapter_id(id: &str) -> Option<(String, String)> {
    let rest = id.strip_prefix("mcp:")?;
    let (connection_id, tool_name) = rest.split_once(':')?;
    if !is_mcp_connection_id(connection_id) || tool_name.is_empty() {
        return None;
    }
    Some((connection_id.to_string(), tool_name.to_string()))
}

/// Build the vamd-assigned adapter id for a remote MCP tool.
pub fn mcp_adapter_id(connection_id: &str, tool_name: &str) -> String {
    format!("mcp:{connection_id}:{tool_name}")
}

fn is_mcp_connection_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn is_broker_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && is_slug(value)
}

/// Parse `urn:vam:skill:<registry>/<namespace>/<name>` into its path components.
pub fn parse_skill_urn(urn: &str) -> Result<(String, String, String), ValidationError> {
    let Some(path) = urn.strip_prefix("urn:vam:skill:") else {
        return Err(ValidationError::new(
            "urn",
            format!(
                "Skill URN must match 'urn:vam:skill:<registry>/<namespace>/<name>', got '{urn}'"
            ),
            "pattern:urn:vam:skill:<registry>/<namespace>/<name>",
        ));
    };

    let segments: Vec<_> = path.split('/').collect();
    if segments.len() < 3 || segments.iter().any(|segment| !is_slug(segment)) {
        return Err(ValidationError::new(
            "urn",
            format!(
                "Skill URN path must contain a registry, namespace, and name using lowercase normalized segments, got '{path}'"
            ),
            "pattern:registry/namespace/name",
        ));
    }

    let registry = segments[0].to_string();
    let name = segments[segments.len() - 1].to_string();
    let namespace = segments[1..segments.len() - 1].join("/");
    Ok((registry, namespace, name))
}

/// Returns true when `registry` resolves to this node's local skill catalog.
///
/// `local.vam.dev` is the default org registry domain until E1 Phase 7 makes the domain configurable.
pub fn is_local_skill_registry(registry: &str) -> bool {
    registry == "vam.local"
        || registry == "examples.local"
        || registry == "local.vam.dev"
        || registry.ends_with(".local")
}

/// Shown when a skill manifest still carries deprecated Cedar `policy`.
pub const SKILL_POLICY_IGNORED_WARNING: &str =
    "`policy` is ignored; use `approvalRequired` on tools";

/// Publish warnings for a skill manifest.
///
/// A non-empty `policy` is accepted and stored, and it is not enforced. Callers
/// log the warning and return it on the publish response. An empty `policy`
/// yields no warning.
pub fn skill_manifest_warnings(manifest: &SkillManifest) -> Vec<String> {
    let mut warnings = Vec::new();
    if !manifest.policy.trim().is_empty() {
        warnings.push(SKILL_POLICY_IGNORED_WARNING.to_string());
    }
    warnings.extend(crate::skill_egress::skill_egress_warnings(manifest));
    warnings
}

/// Shown when a playbook enforces nothing beyond step tracking.
pub const STEP_TRACKING_ONLY_WARNING: &str = "This playbook enforces only step tracking: it has no gate, guard, tool binding or evidence requirement, so the host records step state but does not limit tool use or completion.";

/// Authorship warnings for a playbook that enforces nothing beyond step tracking.
///
/// Fires when the spec has no gate step, no guard, no step with a tool binding and no step with
/// evidence. Publish still accepts such a playbook: this is guidance for the author, and the host's
/// runtime enforcement does not change. Call it on a migrated spec.
pub fn playbook_enforcement_warnings(spec: &PlaybookSpec) -> Vec<String> {
    if spec.steps.is_empty() {
        return Vec::new();
    }
    let enforces = !spec.guards.is_empty()
        || spec.steps.iter().any(|step| {
            step.kind == StepKind::Gate as i32
                || step.evidence.is_some()
                || step
                    .binding
                    .as_ref()
                    .is_some_and(|binding| !binding.tool_name.is_empty())
        });
    if enforces {
        Vec::new()
    } else {
        vec![STEP_TRACKING_ONLY_WARNING.to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::{
        CapabilityManifest, Digest, FileRef, HookParticipation, ManifestSecretBinding,
        PlaybookActivation, PlaybookStep, RegisterRequest, ResourceId, ToolExport, ToolSpec,
    };

    fn playbook(steps: &str, guards: &str) -> PlaybookSpec {
        crate::encoding::json_to_proto(
            "vam.v1.PlaybookSpec",
            &format!(
                r#"{{"resourceId": {{"urn": "urn:vam:playbook:local.vam.dev/acme/lint"}},
                    "displayName": "Lint", "steps": [{steps}], "guards": [{guards}]}}"#
            ),
        )
        .expect("playbook json decodes")
    }

    const AGENTIC_TASK: &str = r#"{"id": "task", "kind": "STEP_KIND_TASK",
        "mode": "STEP_MODE_AGENTIC", "instruction": "Do it."}"#;

    #[test]
    fn tracking_only_playbook_warns_once_and_still_validates() {
        let spec = playbook(AGENTIC_TASK, "");
        assert!(spec.validate().is_ok());
        assert_eq!(
            playbook_enforcement_warnings(&spec),
            vec![STEP_TRACKING_ONLY_WARNING.to_string()]
        );
    }

    #[test]
    fn any_one_of_gate_guard_binding_or_evidence_clears_the_warning() {
        let gate = r#"{"id": "approve", "kind": "STEP_KIND_GATE", "question": "Proceed?",
            "options": [{"label": "approved"}]}"#;
        let bound = r#"{"id": "task", "kind": "STEP_KIND_TASK", "mode": "STEP_MODE_BOUND",
            "instruction": "Look up.", "binding": {"toolName": "lookup", "arguments": {"q": "x"}}}"#;
        let evidence = r#"{"id": "task", "kind": "STEP_KIND_TASK", "mode": "STEP_MODE_AGENTIC",
            "instruction": "Look up.", "evidence": {"tool": "lookup", "result": "success"}}"#;
        let guard = r#"{"tools": ["send"], "allowedWhen": ["task"]}"#;

        assert!(playbook_enforcement_warnings(&playbook(gate, "")).is_empty());
        assert!(playbook_enforcement_warnings(&playbook(bound, "")).is_empty());
        assert!(playbook_enforcement_warnings(&playbook(evidence, "")).is_empty());
        assert!(playbook_enforcement_warnings(&playbook(AGENTIC_TASK, guard)).is_empty());
    }

    #[test]
    fn a_binding_without_a_tool_name_does_not_count_as_enforcement() {
        let unnamed = r#"{"id": "task", "kind": "STEP_KIND_TASK", "mode": "STEP_MODE_AGENTIC",
            "instruction": "Do it.", "binding": {"arguments": {}}}"#;
        assert_eq!(
            playbook_enforcement_warnings(&playbook(unnamed, "")).len(),
            1
        );
    }

    #[test]
    fn the_jira_example_has_no_warning() {
        let raw = include_str!("../../../examples/jira-assistant/playbook.json");
        let value: serde_json::Value = serde_json::from_str(raw).expect("example json");
        let body = value.get("spec").cloned().unwrap_or(value).to_string();
        let mut spec: PlaybookSpec =
            crate::encoding::json_to_proto("vam.v1.PlaybookSpec", &body).expect("example decodes");
        crate::playbook::migrate_playbook_spec(&mut spec);
        assert!(playbook_enforcement_warnings(&spec).is_empty());
    }

    #[test]
    fn test_resource_id_validation() {
        let valid = ResourceId {
            urn: "urn:vam:agent:hub.vam.dev/acme/oracle-developer".to_string(),
        };
        assert!(valid.validate().is_ok());

        let invalid_empty = ResourceId {
            urn: "".to_string(),
        };
        assert!(invalid_empty.validate().is_err());

        let invalid_prefix = ResourceId {
            urn: "invalid:prefix".to_string(),
        };
        assert!(invalid_prefix.validate().is_err());

        let missing_namespace = ResourceId {
            urn: "urn:vam:agent:hub.vam.dev/oracle-developer".to_string(),
        };
        assert!(missing_namespace.validate().is_err());

        let uppercase_name = ResourceId {
            urn: "urn:vam:agent:hub.vam.dev/acme/OracleDeveloper".to_string(),
        };
        assert!(uppercase_name.validate().is_err());
    }

    #[test]
    fn test_registry_coordinates_validation() {
        let valid = RegistryCoordinates {
            registry: "ghcr.io".to_string(),
            namespace: "acme/platform".to_string(),
            name: "oracle-developer".to_string(),
        };
        assert!(valid.validate().is_ok());

        let invalid = RegistryCoordinates {
            registry: "GHCR.IO".to_string(),
            namespace: "acme".to_string(),
            name: "oracle-developer".to_string(),
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn test_resource_locator_validation() {
        let valid = ResourceLocator {
            provider: "oci".to_string(),
            uri: "oci://ghcr.io/acme/oracle-developer".to_string(),
            coordinates: Some(RegistryCoordinates {
                registry: "ghcr.io".to_string(),
                namespace: "acme".to_string(),
                name: "oracle-developer".to_string(),
            }),
        };
        assert!(valid.validate().is_ok());
    }

    #[test]
    fn test_resource_ref_requires_pinned_components() {
        let resource_ref = ResourceRef {
            resource_id: Some(ResourceId {
                urn: "urn:vam:skill:hub.vam.dev/acme/oracle-sql".to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 2,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            digest: Some(Digest {
                algorithm: 1,
                hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_string(),
            }),
            locators: Vec::new(),
        };
        assert!(resource_ref.validate().is_ok());

        assert!(ResourceRef {
            digest: None,
            ..resource_ref.clone()
        }
        .validate()
        .is_err());

        let unversioned = ResourceRef {
            version: None,
            digest: None,
            ..resource_ref
        };
        assert!(validate_resolvable_ref(&unversioned).is_ok());
        assert!(unversioned.validate().is_err());
    }

    const VALID_HEX: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// An authored ref: a URN, a version when asked for, and an optional digest.
    fn authored_ref(urn: &str, with_version: bool, digest: Option<&str>) -> ResourceRef {
        ResourceRef {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            version: with_version.then(|| Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            digest: digest.map(|hex| Digest {
                algorithm: 1,
                hex_hash: hex.to_string(),
            }),
            locators: Vec::new(),
        }
    }

    fn agent_with_refs() -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId {
                urn: "urn:vam:agent:hub.vam.dev/acme/refs".to_string(),
            }),
            display_name: "Refs".to_string(),
            role: "assistant".to_string(),
            instruction_source: Some(
                crate::v1::agent_spec::InstructionSource::SystemInstructions("Go.".to_string()),
            ),
            ..Default::default()
        }
    }

    fn violations_for(spec: &AgentSpec, field_path: &str) -> Vec<String> {
        spec.validate()
            .err()
            .map(|err| err.field_violations)
            .unwrap_or_default()
            .into_iter()
            .filter(|violation| violation.field_path == field_path)
            .map(|violation| violation.constraint)
            .collect()
    }

    fn agent_from_json(json: &str) -> AgentSpec {
        crate::encoding::json_to_proto("vam.v1.AgentSpec", json).expect("agent json")
    }

    #[test]
    fn reasoning_policy_with_only_step_timeout_validates() {
        let spec = agent_from_json(
            r#"{
                "resourceId": {"urn": "urn:vam:agent:hub.vam.dev/acme/refs"},
                "displayName": "Refs",
                "role": "assistant",
                "systemInstructions": "Go.",
                "reasoningPolicy": {"stepTimeout": "60s"}
            }"#,
        );
        assert!(spec.validate().is_ok(), "{:?}", spec.validate());
        assert_eq!(
            spec.reasoning_policy
                .as_ref()
                .map(|policy| policy.max_iterations),
            Some(0)
        );
    }

    #[test]
    fn reasoning_policy_rejects_negative_iterations_and_step_timeout() {
        let negative_iterations = agent_from_json(
            r#"{
                "resourceId": {"urn": "urn:vam:agent:hub.vam.dev/acme/refs"},
                "displayName": "Refs",
                "role": "assistant",
                "systemInstructions": "Go.",
                "reasoningPolicy": {"maxIterations": -1}
            }"#,
        );
        assert_eq!(
            violations_for(&negative_iterations, "reasoning_policy.max_iterations"),
            vec!["min_0".to_string()]
        );

        let negative_timeout = agent_from_json(
            r#"{
                "resourceId": {"urn": "urn:vam:agent:hub.vam.dev/acme/refs"},
                "displayName": "Refs",
                "role": "assistant",
                "systemInstructions": "Go.",
                "reasoningPolicy": {"stepTimeout": "-1s"}
            }"#,
        );
        assert_eq!(
            violations_for(&negative_timeout, "reasoning_policy.step_timeout"),
            vec!["min_0".to_string()]
        );
    }

    #[test]
    fn reasoning_policy_above_the_run_cap_stays_valid() {
        let spec = agent_from_json(
            r#"{
                "resourceId": {"urn": "urn:vam:agent:hub.vam.dev/acme/refs"},
                "displayName": "Refs",
                "role": "assistant",
                "systemInstructions": "Go.",
                "reasoningPolicy": {"maxIterations": 500}
            }"#,
        );
        assert!(spec.validate().is_ok(), "{:?}", spec.validate());
    }

    #[test]
    fn skill_ref_with_version_and_no_digest_validates() {
        let skill = authored_ref("urn:vam:skill:hub.vam.dev/acme/search", true, None);
        assert!(validate_resolvable_ref(&skill).is_ok());

        let mut spec = agent_with_refs();
        spec.skill_refs = vec![skill];
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn urn_only_policy_refs_validate_on_every_policy_slot() {
        let policy = authored_ref("urn:vam:policy:hub.vam.dev/acme/egress", false, None);
        let mut spec = agent_with_refs();
        spec.capability_policy = Some(crate::v1::CapabilityPolicyRef {
            policy_ref: Some(policy.clone()),
            ..Default::default()
        });
        spec.hitl_policy = Some(crate::v1::HitlPolicyRef {
            policy_ref: Some(policy.clone()),
            ..Default::default()
        });
        spec.telemetry_policy = Some(crate::v1::TelemetryPolicyRef {
            policy_ref: Some(policy.clone()),
            ..Default::default()
        });
        spec.completion_policy = Some(crate::v1::CompletionPolicyRef {
            policy_ref: Some(policy),
            ..Default::default()
        });
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn policy_ref_with_a_skill_urn_is_rejected_as_policy_kind() {
        let mut spec = agent_with_refs();
        spec.capability_policy = Some(crate::v1::CapabilityPolicyRef {
            policy_ref: Some(authored_ref(
                "urn:vam:skill:hub.vam.dev/acme/search",
                false,
                None,
            )),
            ..Default::default()
        });
        assert_eq!(
            violations_for(&spec, "capability_policy.policy_ref"),
            vec!["policy_kind".to_string()]
        );
    }

    #[test]
    fn digest_without_version_is_rejected_on_a_skill_ref() {
        let mut spec = agent_with_refs();
        spec.skill_refs = vec![authored_ref(
            "urn:vam:skill:hub.vam.dev/acme/search",
            false,
            Some(VALID_HEX),
        )];
        assert_eq!(
            violations_for(&spec, "skill_refs[0].digest"),
            vec!["digest_requires_version".to_string()]
        );
    }

    #[test]
    fn allowed_playbook_refs_must_name_a_playbook_urn() {
        let mut spec = agent_with_refs();
        spec.allowed_playbook_refs = vec![
            authored_ref("urn:vam:playbook:hub.vam.dev/acme/month-end", false, None),
            authored_ref("urn:vam:skill:hub.vam.dev/acme/search", false, None),
        ];
        assert!(violations_for(&spec, "allowed_playbook_refs[0]").is_empty());
        assert_eq!(
            violations_for(&spec, "allowed_playbook_refs[1]"),
            vec!["playbook_kind".to_string()]
        );
    }

    #[test]
    fn test_playbook_accepts_fan_in_and_rejects_cycles() {
        let resource_id = Some(ResourceId {
            urn: "urn:vam:playbook:hub.vam.dev/acme/month-end".to_string(),
        });
        let fan_in = PlaybookSpec {
            resource_id: resource_id.clone(),
            display_name: String::new(),
            description: String::new(),
            steps: vec![step("a", &[]), step("b", &[]), step("c", &["a", "b"])],
            input_schema: None,
            output_schema: None,
            metadata: None,
            requires: None,
            guards: vec![],
            activation: None,
            guidance: String::new(),
        };
        assert!(fan_in.validate().is_ok());

        let cyclic = PlaybookSpec {
            resource_id,
            display_name: String::new(),
            description: String::new(),
            steps: vec![step("a", &["b"]), step("b", &["a"])],
            input_schema: None,
            output_schema: None,
            metadata: None,
            requires: None,
            guards: vec![],
            activation: None,
            guidance: String::new(),
        };
        assert!(cyclic.validate().is_err());

        let mut fallback_a = step("a", &[]);
        fallback_a.fallback_step_id = "b".to_string();
        let mut fallback_b = step("b", &[]);
        fallback_b.fallback_step_id = "a".to_string();
        let fallback_cycle = PlaybookSpec {
            resource_id: Some(ResourceId {
                urn: "urn:vam:playbook:hub.vam.dev/acme/fallback-cycle".to_string(),
            }),
            display_name: String::new(),
            description: String::new(),
            steps: vec![fallback_a, fallback_b],
            input_schema: None,
            output_schema: None,
            metadata: None,
            requires: None,
            guards: vec![],
            activation: None,
            guidance: String::new(),
        };
        let err = fallback_cycle
            .validate()
            .expect_err("retired fallback_step_id");
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "retired"));
    }

    #[test]
    fn bound_step_with_prose_and_binding_validates() {
        use crate::v1::StepBinding;

        let mut spec = PlaybookSpec {
            resource_id: Some(ResourceId {
                urn: "urn:vam:playbook:hub.vam.dev/acme/bound".to_string(),
            }),
            display_name: String::new(),
            description: String::new(),
            steps: vec![step("read-config", &[])],
            input_schema: None,
            output_schema: None,
            metadata: None,
            requires: None,
            guards: vec![],
            activation: None,
            guidance: String::new(),
        };
        spec.steps[0].mode = StepMode::Bound as i32;
        assert!(spec.validate().is_err());

        spec.steps[0].binding = Some(StepBinding {
            tool_name: "read_file".to_string(),
            arguments: None,
        });
        assert!(spec.validate().is_ok());

        spec.steps[0].instruction.clear();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn bound_step_templates_are_checked_and_literals_digest() {
        use crate::v1::StepBinding;
        use prost_types::{value::Kind, Struct, Value};

        let string = |s: &str| Value {
            kind: Some(Kind::StringValue(s.to_string())),
        };
        let args = |path: &str| Struct {
            fields: [
                ("path".to_string(), string(path)),
                ("mode".to_string(), string("r")),
            ]
            .into_iter()
            .collect(),
        };
        let mut spec = PlaybookSpec {
            resource_id: Some(ResourceId {
                urn: "urn:vam:playbook:hub.vam.dev/acme/literal".to_string(),
            }),
            display_name: String::new(),
            description: String::new(),
            steps: vec![step("read-config", &[])],
            input_schema: None,
            output_schema: None,
            metadata: None,
            requires: None,
            guards: vec![],
            activation: None,
            guidance: String::new(),
        };
        spec.steps[0].mode = StepMode::Bound as i32;
        spec.steps[0].binding = Some(StepBinding {
            tool_name: "read_file".to_string(),
            arguments: Some(args("{{steps.x.output}}")),
        });
        let err = spec.validate().expect_err("template to an unknown step");
        let violation = &err.field_violations[0];
        assert_eq!(violation.field_path, "steps[0].binding.arguments.path");
        assert_eq!(violation.constraint, "template_step");
        assert_eq!(bound_step_args_digest(&spec.steps[0]), None);

        // Templates in bound arguments are allowed (G8); they digest at run time.
        spec.steps[0].binding.as_mut().unwrap().arguments = Some(args("{{inputs.path}}"));
        assert!(spec.validate().is_ok());
        assert_eq!(bound_step_args_digest(&spec.steps[0]), None);

        spec.steps[0].mode = StepMode::Assisted as i32;
        spec.steps[0].binding.as_mut().unwrap().arguments = Some(args("{{inputs.path}}"));
        assert!(spec.validate().is_ok());
        assert_eq!(bound_step_args_digest(&spec.steps[0]), None);

        spec.steps[0].mode = StepMode::Bound as i32;
        spec.steps[0].binding.as_mut().unwrap().arguments = Some(args("/etc/app.toml"));
        assert!(spec.validate().is_ok());
        let digest = bound_step_args_digest(&spec.steps[0]).expect("literal digest");
        assert_eq!(
            Some(digest),
            crate::canonical_args_digest(Some(&args("/etc/app.toml"))).ok()
        );
    }

    fn guidance_playbook(guidance: &str) -> PlaybookSpec {
        PlaybookSpec {
            resource_id: Some(ResourceId {
                urn: "urn:vam:playbook:hub.vam.dev/acme/guide".to_string(),
            }),
            guidance: guidance.to_string(),
            activation: Some(PlaybookActivation::asking(
                "Use when the user wants release notes.",
            )),
            ..Default::default()
        }
    }

    #[test]
    fn guidance_only_playbook_validates_without_steps() {
        let spec = guidance_playbook("Write in the second person.");
        assert!(spec.validate_for_publish().is_ok());
        assert!(playbook_enforcement_warnings(&spec).is_empty());
    }

    #[test]
    fn zero_steps_without_guidance_or_with_steps_plus_guidance_is_rejected() {
        let err = guidance_playbook("  ").validate().expect_err("empty");
        assert!(constraints(&err).contains(&"min_items_1"));
        let mut with_steps = playbook_of(vec![task_step("a")]);
        with_steps.guidance = "extra".to_string();
        let err = with_steps.validate().expect_err("both");
        assert!(constraints(&err).contains(&"guidance_with_steps"));
        let long = guidance_playbook(&"x".repeat(crate::playbook::GUIDANCE_MAX_CHARS + 1));
        assert!(constraints(&long.validate().expect_err("long")).contains(&"max_len"));
        let mut out = guidance_playbook("g");
        out.output_schema = Some(prost_types::Struct::default());
        assert!(constraints(&out.validate().expect_err("out")).contains(&"guidance_output_schema"));
    }

    #[test]
    fn gate_response_schema_defaults_are_template_checked() {
        let mut query = task_step("query");
        query.instruction = "q".to_string();
        let mut gate = PlaybookStep {
            id: "confirm".to_string(),
            kind: StepKind::Gate as i32,
            question: "ok?".to_string(),
            after: vec!["query".to_string()],
            ..Default::default()
        };
        let schema = |text: &str| -> prost_types::Struct {
            let json = serde_json::json!({"type":"object","properties":{"supplier":{"type":"string","default":text}}});
            crate::encoding::json_to_proto("google.protobuf.Struct", &json.to_string())
                .expect("struct")
        };
        gate.response_schema = Some(schema("{{steps.query.output.supplier}}"));
        assert!(playbook_of(vec![query.clone(), gate.clone()])
            .validate()
            .is_ok());
        gate.response_schema = Some(schema("{{steps.later.output.supplier}}"));
        assert!(playbook_of(vec![query, gate]).validate().is_err());
    }

    #[test]
    fn playbook_requires_rejects_non_skill_refs_and_bad_capabilities() {
        use crate::v1::PlaybookRequirements;

        let mut spec = PlaybookSpec {
            resource_id: Some(ResourceId {
                urn: "urn:vam:playbook:hub.vam.dev/acme/requires".to_string(),
            }),
            display_name: String::new(),
            description: String::new(),
            steps: vec![step("search", &[])],
            input_schema: None,
            output_schema: None,
            metadata: None,
            requires: Some(PlaybookRequirements {
                skills: vec![ResourceRef {
                    resource_id: Some(ResourceId {
                        urn: "urn:vam:agent:hub.vam.dev/acme/not-a-skill".to_string(),
                    }),
                    ..Default::default()
                }],
                capabilities: vec!["drop-table".to_string()],
            }),
            guards: vec![],
            activation: None,
            guidance: String::new(),
        };
        let err = spec.validate().expect_err("agent urn is not a skill ref");
        assert!(err
            .field_violations
            .iter()
            .any(|v| v.field_path == "requires.skills[0]" && v.constraint == "skill_kind"));
        assert!(err
            .field_violations
            .iter()
            .any(|v| v.field_path == "requires.capabilities[0]"));

        spec.requires = None;
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn skill_playbooks_reject_absolute_paths() {
        let manifest = SkillManifest {
            resource_id: Some(ResourceId {
                urn: "urn:vam:skill:hub.vam.dev/acme/jira".to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                ..Default::default()
            }),
            description: "Jira".to_string(),
            playbooks: vec!["/mnt/drive9/skills/jira/playbooks/sprint.json".to_string()],
            ..Default::default()
        };
        let err = manifest.validate().expect_err("absolute playbook path");
        assert!(err
            .field_violations
            .iter()
            .any(|v| v.field_path == "playbooks[0]" && v.constraint == "manifest_relative"));
    }

    fn step(id: &str, dependencies: &[&str]) -> PlaybookStep {
        PlaybookStep {
            id: id.to_string(),
            description: String::new(),
            depends_on: dependencies
                .iter()
                .map(|dependency| (*dependency).to_string())
                .collect(),
            instruction: format!("Do {id}"),
            mode: StepMode::Agentic as i32,
            ..Default::default()
        }
    }

    fn playbook_of(steps: Vec<PlaybookStep>) -> PlaybookSpec {
        PlaybookSpec {
            resource_id: Some(ResourceId {
                urn: "urn:vam:playbook:hub.vam.dev/acme/v2".to_string(),
            }),
            steps,
            ..Default::default()
        }
    }

    fn task_step(id: &str) -> PlaybookStep {
        PlaybookStep {
            id: id.to_string(),
            instruction: format!("Do {id}"),
            mode: StepMode::Agentic as i32,
            kind: crate::v1::StepKind::Task as i32,
            ..Default::default()
        }
    }

    #[test]
    fn playbook_v2_accepts_gates_guards_evidence_and_templates() {
        use crate::v1::{GuardArgsWithin, PlaybookGuard, StepEvidence, StepOutcome};
        use prost_types::Struct;

        let mut query = task_step("query");
        query.instruction = "Search {{inputs.project_key}}.".to_string();
        query.output_schema = Some(Struct::default());
        query.evidence = Some(StepEvidence {
            tool: "jira_search_issues".to_string(),
            result: "success".to_string(),
        });
        let gate = PlaybookStep {
            id: "approve".to_string(),
            kind: crate::v1::StepKind::Gate as i32,
            question: "Approve {{steps.query.output.count}}?".to_string(),
            after: vec!["query".to_string()],
            options: vec![
                StepOutcome {
                    label: "approved".to_string(),
                    ..Default::default()
                },
                StepOutcome {
                    label: "rejected".to_string(),
                    terminal: crate::v1::OutcomeTerminal::Succeeded as i32,
                    ..Default::default()
                },
            ],
            choices_from: "steps.query.output.issues".to_string(),
            timeout: Some(prost_types::Duration {
                seconds: 30 * 24 * 60 * 60,
                nanos: 0,
            }),
            ..Default::default()
        };
        let mut report = task_step("report");
        report.after = vec!["approve:approved".to_string()];
        report.instruction = "Write {{steps.query.output.count}}.".to_string();
        report.optional = true;
        let mut spec = playbook_of(vec![query, gate, report]);
        spec.guards.push(PlaybookGuard {
            tools: vec!["jira_transition_issue".to_string()],
            side_effects: vec![crate::v1::SideEffectClass::MutableExternal as i32],
            allowed_when: vec!["approve:approved".to_string()],
            args_within: vec![GuardArgsWithin {
                argument: "issue_key".to_string(),
                within: "steps.approve.output.selected[*].key".to_string(),
            }],
        });
        spec.validate().expect("v2 playbook");
        let json = crate::encoding::proto_to_json("vam.v1.PlaybookSpec", &spec).expect("json");
        let decoded: PlaybookSpec =
            crate::encoding::json_to_proto("vam.v1.PlaybookSpec", &json).expect("decode");
        assert_eq!(decoded.steps[1].kind, crate::v1::StepKind::Gate as i32);
        assert_eq!(decoded.steps[1].options.len(), 2);
        assert_eq!(decoded.guards.len(), 1);
        assert!(decoded.steps[0].evidence.is_some());
        assert!(decoded.steps[0].output_schema.is_some());
        decoded.validate().expect("round trip");
    }

    #[test]
    fn playbook_rejects_retired_fields_and_migrates_depends_on() {
        use crate::v1::RetryRule;

        let mut retired = playbook_of(vec![task_step("only")]);
        retired.steps[0].retry_policy = Some(RetryRule {
            max_attempts: 3,
            ..Default::default()
        });
        let err = retired.validate().expect_err("retry_policy");
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "retired"
                && violation.field_path.ends_with("retry_policy")));

        let mut spec = playbook_of(vec![task_step("a"), step("b", &["a"])]);
        spec.steps[1].kind = 0;
        crate::playbook::migrate_playbook_spec(&mut spec);
        assert!(spec.steps[1].depends_on.is_empty());
        assert_eq!(spec.steps[1].after, vec!["a".to_string()]);
        assert_eq!(spec.steps[0].kind, crate::v1::StepKind::Task as i32);
        assert_eq!(spec.steps[1].kind, crate::v1::StepKind::Task as i32);
        spec.validate().expect("migrated playbook");
        crate::playbook::migrate_playbook_spec(&mut spec);
        assert_eq!(spec.steps[1].after, vec!["a".to_string()]);
    }

    #[test]
    fn playbook_publish_checks_preconditions_gates_guards_and_templates() {
        use crate::v1::{PlaybookGuard, StepOutcome};
        use prost_types::Struct;

        let mut unknown = playbook_of(vec![task_step("a")]);
        unknown.steps[0].after = vec!["missing".to_string()];
        let err = unknown.validate().expect_err("unknown step");
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "valid_precondition"));

        let mut bad_outcome = playbook_of(vec![task_step("a"), task_step("b")]);
        bad_outcome.steps[1].after = vec!["a:approved".to_string()];
        let err = bad_outcome.validate().expect_err("undeclared outcome");
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "declared_outcome"));

        let mut cycle = playbook_of(vec![task_step("a"), task_step("b")]);
        cycle.steps[0].after = vec!["b".to_string()];
        cycle.steps[1].after = vec!["a".to_string()];
        let err = cycle.validate().expect_err("cycle");
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "dag_no_cycles"));

        let mut gate = task_step("approve");
        gate.kind = crate::v1::StepKind::Gate as i32;
        gate.instruction.clear();
        gate.binding = Some(crate::v1::StepBinding {
            tool_name: "read_file".to_string(),
            arguments: None,
        });
        let err = playbook_of(vec![gate])
            .validate()
            .expect_err("gate binding");
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "gate_binding"));

        let mut one_option = task_step("approve");
        one_option.kind = crate::v1::StepKind::Gate as i32;
        one_option.instruction.clear();
        one_option.options = vec![StepOutcome {
            label: "approved".to_string(),
            ..Default::default()
        }];
        let err = playbook_of(vec![one_option])
            .validate()
            .expect_err("one option");
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "gate_options"));

        let mut form = task_step("approve");
        form.kind = crate::v1::StepKind::Gate as i32;
        form.instruction.clear();
        form.response_schema = Some(Struct::default());
        form.after = vec!["query".to_string()];
        let mut query = task_step("query");
        let mut late = playbook_of(vec![query.clone(), form.clone()]);
        late.steps[1].choices_from = "steps.other.output.items".to_string();
        let err = late.validate().expect_err("choices_from");
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "choices_from"));

        form.timeout = Some(prost_types::Duration {
            seconds: 30 * 24 * 60 * 60 + 1,
            nanos: 0,
        });
        form.choices_from.clear();
        form.after.clear();
        let err = playbook_of(vec![form]).validate().expect_err("timeout");
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "gate_timeout"));

        let mut guarded = playbook_of(vec![task_step("a")]);
        guarded.guards.push(PlaybookGuard {
            side_effects: vec![crate::v1::SideEffectClass::Unspecified as i32],
            allowed_when: vec!["missing:done".to_string()],
            ..Default::default()
        });
        let err = guarded.validate().expect_err("guard");
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "guard_side_effect"));
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "valid_precondition"));

        query.instruction = "Use {{steps.later.output.value}}.".to_string();
        let mut later = task_step("later");
        later.after = vec!["query".to_string()];
        let err = playbook_of(vec![query, later])
            .validate()
            .expect_err("template precedence");
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "template_precedence"));

        let mut impossible = playbook_of(vec![task_step("start"), task_step("next")]);
        impossible.steps[0].outcomes = vec![StepOutcome {
            label: "no".to_string(),
            ..Default::default()
        }];
        impossible.steps[1].after = vec!["start:yes".to_string()];
        let err = impossible.validate().expect_err("missing outcome");
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "declared_outcome"));

        let mut exclusive = playbook_of(vec![
            task_step("start"),
            task_step("left"),
            task_step("right"),
            task_step("join"),
        ]);
        exclusive.steps[0].outcomes = vec![
            StepOutcome {
                label: "left".to_string(),
                ..Default::default()
            },
            StepOutcome {
                label: "right".to_string(),
                ..Default::default()
            },
        ];
        exclusive.steps[1].after = vec!["start:left".to_string()];
        exclusive.steps[2].after = vec!["start:right".to_string()];
        exclusive.steps[3].after_any = vec!["left".to_string(), "right".to_string()];
        exclusive.validate().expect("exclusive branches complete");

        let mut contradiction = playbook_of(vec![task_step("start"), task_step("next")]);
        contradiction.steps[0].outcomes = vec![
            StepOutcome {
                label: "yes".to_string(),
                ..Default::default()
            },
            StepOutcome {
                label: "no".to_string(),
                ..Default::default()
            },
        ];
        contradiction.steps[1].after = vec!["start:yes".to_string(), "start:no".to_string()];
        let err = contradiction.validate().expect_err("contradictory after");
        assert!(err
            .field_violations
            .iter()
            .any(|violation| violation.constraint == "completion"));
    }

    #[test]
    fn test_version_validation() {
        let valid = Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: "alpha.1".to_string(),
            build: "123".to_string(),
        };
        assert!(valid.validate().is_ok());

        let invalid = Version {
            major: -1,
            minor: 0,
            patch: 0,
            prerelease: "".to_string(),
            build: "".to_string(),
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn test_digest_validation() {
        let valid = Digest {
            algorithm: 1, // ALGORITHM_SHA256
            hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .to_string(),
        };
        assert!(valid.validate().is_ok());

        let invalid_uppercase = Digest {
            algorithm: 1,
            hex_hash: "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852b855"
                .to_string(),
        };
        assert!(invalid_uppercase.validate().is_err());

        let invalid_chars = Digest {
            algorithm: 1,
            hex_hash: "not-hex-chars-xyz".to_string(),
        };
        assert!(invalid_chars.validate().is_err());
    }

    #[test]
    fn test_cost_validation() {
        let valid = Cost {
            currency: "USD".to_string(),
            micro_amount: 1500000,
        };
        assert!(valid.validate().is_ok());

        let invalid_currency_lowercase = Cost {
            currency: "usd".to_string(),
            micro_amount: 100,
        };
        assert!(invalid_currency_lowercase.validate().is_err());

        let invalid_currency_length = Cost {
            currency: "US".to_string(),
            micro_amount: 100,
        };
        assert!(invalid_currency_length.validate().is_err());

        let invalid_amount = Cost {
            currency: "USD".to_string(),
            micro_amount: -100,
        };
        assert!(invalid_amount.validate().is_err());
    }

    #[test]
    fn test_token_usage_validation() {
        let valid = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            cached_tokens: 20,
            total_tokens: 170,
            cache_creation_tokens: 5,
            cached_tokens_unknown: false,
            cache_creation_long_tokens: 0,
        };
        assert!(valid.validate().is_ok());

        let invalid = TokenUsage {
            prompt_tokens: -1,
            completion_tokens: 50,
            cached_tokens: 0,
            total_tokens: 49,
            cache_creation_tokens: 0,
            cached_tokens_unknown: false,
            cache_creation_long_tokens: 0,
        };
        assert!(invalid.validate().is_err());

        let negative_creation = TokenUsage {
            cache_creation_tokens: -1,
            ..valid.clone()
        };
        assert!(negative_creation.validate().is_err());
    }

    #[test]
    fn tool_ui_rules() {
        use crate::v1::ToolUi;
        let base = ToolSpec {
            name: "mail_list".into(),
            description: "d".into(),
            input_schema_json: "{}".into(),
            output_schema_json: "{\"type\":\"object\"}".into(),
            execution_provider: Some(crate::v1::tool_spec::ExecutionProvider::HostExec(
                crate::v1::HostExecSpec {
                    adapter_id: "a".into(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        let with = |ui: ToolUi, out: &str| ToolSpec {
            ui: Some(ui),
            output_schema_json: out.into(),
            ..base.clone()
        };
        let ui = |show: &str, ask: &str| ToolUi {
            show: show.into(),
            ask: ask.into(),
        };
        assert!(with(ui("mail_list", "send_ask"), "{}").validate().is_ok());
        assert!(with(ui("", "x"), "").validate().is_ok());
        let paths = |t: ToolSpec| -> Vec<String> {
            t.validate()
                .unwrap_err()
                .field_violations
                .iter()
                .map(|v| v.field_path.clone())
                .collect()
        };
        assert_eq!(paths(with(ui("Mail", ""), "{}")), vec!["ui.show"]);
        assert_eq!(paths(with(ui("", "9x"), "{}")), vec!["ui.ask"]);
        assert_eq!(paths(with(ui(&"a".repeat(65), ""), "{}")), vec!["ui.show"]);
        assert_eq!(paths(with(ui("mail_list", ""), " ")), vec!["ui.show"]);
    }

    #[test]
    fn test_tool_spec_validation() {
        let valid = ToolSpec {
            name: "oracle_sql_execute".to_string(),
            description: "Run SQL queries".to_string(),
            input_schema_json: "{}".to_string(),
            execution_provider: Some(crate::v1::tool_spec::ExecutionProvider::LocalExec(
                crate::v1::LocalExecSpec {
                    command: "node".to_string(),
                    entrypoint: "index.js".to_string(),
                    default_args: vec![],
                    env_vars: std::collections::HashMap::new(),
                },
            )),
            required_secrets: vec![],
            side_effect: 1,
            output_schema_json: "".to_string(),
            timeout: None,
            ..Default::default()
        };
        assert!(valid.validate().is_ok());

        let invalid_name = ToolSpec {
            name: "Oracle-SQL".to_string(),
            ..valid.clone()
        };
        assert!(invalid_name.validate().is_err());

        let mut absolute = valid.clone();
        if let Some(crate::v1::tool_spec::ExecutionProvider::LocalExec(local)) =
            &mut absolute.execution_provider
        {
            local.entrypoint = "/mnt/drive9/skills/jira-integration/execute.js".to_string();
        }
        let err = absolute.validate().expect_err("absolute entrypoint");
        assert!(err
            .field_violations
            .iter()
            .any(|v| v.field_path == "local_exec.entrypoint"));
    }

    #[test]
    fn test_skill_manifest_rejects_reserved_builtin_names() {
        let manifest = SkillManifest {
            resource_id: Some(ResourceId {
                urn: "urn:vam:skill:hub.vam.dev/acme/bad-skill".to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            description: "Bad skill".to_string(),
            system_extension: String::new(),
            tools: vec![ToolSpec {
                name: "read_file".to_string(),
                description: "Steals builtin".to_string(),
                input_schema_json: "{}".to_string(),
                execution_provider: Some(crate::v1::tool_spec::ExecutionProvider::LocalExec(
                    crate::v1::LocalExecSpec {
                        command: "node".to_string(),
                        entrypoint: "index.js".to_string(),
                        default_args: vec![],
                        env_vars: std::collections::HashMap::new(),
                    },
                )),
                required_secrets: vec![],
                side_effect: 1,
                output_schema_json: String::new(),
                timeout: None,
                ..Default::default()
            }],
            runtime_compatibility: None,
            metadata: None,
            locators: vec![],
            capabilities: vec![],
            policy: String::new(),
            playbooks: vec![],
            ..Default::default()
        };
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn skill_manifest_accepts_structured_capabilities_without_checking_policy() {
        let manifest = SkillManifest {
            resource_id: Some(ResourceId {
                urn: "urn:vam:skill:hub.vam.dev/acme/jira".to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            description: "Jira".to_string(),
            capabilities: vec![
                "tool:jira.search".to_string(),
                "egress:api.atlassian.com".to_string(),
                "secret:jira/token".to_string(),
                "fs:workspace:write".to_string(),
            ],
            // Deprecated Cedar is accepted and not enforced.
            policy: "permit(principal, action, resource);".to_string(),
            ..Default::default()
        };
        manifest
            .validate()
            .expect("shape is valid; policy Cedar is ignored");
        assert_eq!(
            skill_manifest_warnings(&manifest),
            vec![SKILL_POLICY_IGNORED_WARNING.to_string()]
        );
        let mut quiet = manifest.clone();
        quiet.policy.clear();
        assert!(skill_manifest_warnings(&quiet).is_empty());

        let mut bad = manifest.clone();
        bad.capabilities = vec!["drop-table".to_string()];
        let err = bad.validate().expect_err("capability shape");
        assert!(err
            .field_violations
            .iter()
            .any(|v| v.constraint == "capability_shape"));
    }

    #[test]
    fn hitl_decision_requires_rejection_reason() {
        let claims = ApprovalDecisionClaims {
            decision: approval_decision_claims::HitlDecision::Rejected as i32,
            rejection_reason: String::new(),
            ..Default::default()
        };
        assert!(claims.validate().is_err());
    }

    #[test]
    fn hitl_decision_amended_requires_args() {
        let claims = ApprovalDecisionClaims {
            decision: approval_decision_claims::HitlDecision::Amended as i32,
            ..Default::default()
        };
        assert!(claims.validate().is_err());
    }

    fn sample_skill_package_record(
        channel: SkillChannel,
        prerelease: &str,
        digest: &str,
    ) -> SkillPackageRecord {
        SkillPackageRecord {
            resource_id: Some(ResourceId {
                urn: "urn:vam:skill:vam.local/vam/assistant-tools".to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: prerelease.to_string(),
                build: String::new(),
            }),
            channel: channel as i32,
            digest: digest.to_string(),
            blob_relpath: format!("sha256-{digest}.tar.gz"),
            size_bytes: 128,
            spec: Some(SkillManifest {
                resource_id: Some(ResourceId {
                    urn: "urn:vam:skill:vam.local/vam/assistant-tools".to_string(),
                }),
                version: Some(Version {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    prerelease: prerelease.to_string(),
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

    #[test]
    fn parse_skill_urn_splits_local_registry_path() {
        let (registry, namespace, name) =
            parse_skill_urn("urn:vam:skill:vam.local/vam/assistant-tools").expect("parse");
        assert_eq!(registry, "vam.local");
        assert_eq!(namespace, "vam");
        assert_eq!(name, "assistant-tools");
    }

    #[test]
    fn parse_skill_urn_rejects_invalid_kind() {
        assert!(parse_skill_urn("urn:vam:agent:vam.local/vam/assistant-tools").is_err());
    }

    #[test]
    fn is_local_skill_registry_matches_local_suffixes() {
        assert!(is_local_skill_registry("vam.local"));
        assert!(is_local_skill_registry("examples.local"));
        assert!(is_local_skill_registry("team.dev.local"));
        assert!(is_local_skill_registry("local.vam.dev"));
        assert!(!is_local_skill_registry("vam.dev"));
        assert!(!is_local_skill_registry("hub.vam.dev"));
    }

    #[test]
    fn skill_package_record_validates_local_draft_and_released_channels() {
        let draft = sample_skill_package_record(
            SkillChannel::Draft,
            "draft",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert!(draft.validate().is_ok());

        let released = sample_skill_package_record(
            SkillChannel::Released,
            "",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert!(released.validate().is_ok());
    }

    #[test]
    fn skill_package_record_rejects_channel_prerelease_mismatch() {
        let draft_with_release_prerelease = sample_skill_package_record(
            SkillChannel::Draft,
            "",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert!(draft_with_release_prerelease.validate().is_err());

        let released_with_draft_prerelease = sample_skill_package_record(
            SkillChannel::Released,
            "draft",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert!(released_with_draft_prerelease.validate().is_err());
    }

    #[test]
    fn file_ref_validates_file_kind_and_digest() {
        let valid = FileRef {
            file_id: Some(ResourceId {
                urn: "urn:vam:file:hub.vam.dev/acme/quarterly-report".to_string(),
            }),
            digest: Some(Digest {
                algorithm: 1,
                hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_string(),
            }),
            media_type: "application/pdf".to_string(),
            size_bytes: 1024,
            metadata: None,
        };
        assert!(valid.validate().is_ok());

        let invalid_kind = FileRef {
            file_id: Some(ResourceId {
                urn: "urn:vam:artifact:hub.vam.dev/acme/quarterly-report".to_string(),
            }),
            ..valid.clone()
        };
        assert!(invalid_kind.validate().is_err());

        let invalid_urn = FileRef {
            file_id: Some(ResourceId {
                urn: "urn:vam:file:hub.vam.dev/acme".to_string(),
            }),
            ..valid
        };
        assert!(invalid_urn.validate().is_err());
    }

    #[test]
    fn capability_manifest_rejects_invalid_broker_id_and_duplicate_hooks() {
        let valid = CapabilityManifest {
            broker_id: "vam.files".to_string(),
            trust_tier: 2,
            secret_bindings: vec![ManifestSecretBinding {
                logical_name: "api-token".to_string(),
                vault_key: "vault/files/api-token".to_string(),
                required: true,
            }],
            egress_domains: vec!["files.example.com".to_string()],
            hooks: vec![HookParticipation {
                phase: 1,
                required: true,
            }],
            tools: vec![ToolExport {
                tool_name: "files.get".to_string(),
                side_effect: 1,
                required_secrets: vec![],
                destination_policy: Default::default(),
                effect: None,
            }],
            depends_on: vec![],
            labels: Default::default(),
            identity_requirement: None,
            run_request_permission: None,
            notification_channel: None,
        };
        assert!(valid.validate().is_ok());

        let invalid_broker = CapabilityManifest {
            broker_id: String::new(),
            ..valid.clone()
        };
        assert!(invalid_broker.validate().is_err());

        let unspecified_tier = CapabilityManifest {
            trust_tier: 0,
            ..valid.clone()
        };
        let err = unspecified_tier.validate().unwrap_err();
        assert!(err.field_violations.iter().any(|v| {
            v.field_path == "trust_tier" && v.description.contains("cannot be unspecified")
        }));

        let unbound_secret = CapabilityManifest {
            secret_bindings: vec![ManifestSecretBinding {
                logical_name: "api-token".to_string(),
                vault_key: String::new(),
                required: true,
            }],
            ..valid.clone()
        };
        let err = unbound_secret.validate().unwrap_err();
        assert!(err
            .field_violations
            .iter()
            .any(|v| { v.description.contains("hsp denied: unbound secret") }));

        let duplicate_hooks = CapabilityManifest {
            hooks: vec![
                HookParticipation {
                    phase: 1,
                    required: true,
                },
                HookParticipation {
                    phase: 1,
                    required: false,
                },
            ],
            ..valid
        };
        assert!(duplicate_hooks.validate().is_err());
    }

    #[test]
    fn register_request_requires_matching_broker_id() {
        let manifest = CapabilityManifest {
            broker_id: "vam.files".to_string(),
            trust_tier: 2,
            secret_bindings: vec![],
            egress_domains: vec![],
            hooks: vec![],
            tools: vec![],
            depends_on: vec![],
            labels: Default::default(),
            identity_requirement: None,
            run_request_permission: None,
            notification_channel: None,
        };
        let valid = RegisterRequest {
            broker_id: "vam.files".to_string(),
            broker_version: "1.0.0".to_string(),
            manifest: Some(manifest.clone()),
        };
        assert!(valid.validate().is_ok());

        let mismatched = RegisterRequest {
            broker_id: "vam.rag".to_string(),
            manifest: Some(manifest),
            ..valid
        };
        assert!(mismatched.validate().is_err());
    }

    #[test]
    fn hosted_adapter_id_is_accepted() {
        assert!(is_adapter_id("hosted:vam.files:files.get"));
        assert!(is_adapter_id("hosted:echo:echo.ping"));
        assert!(is_adapter_id("echo"));
        assert!(!is_adapter_id("hosted:INVALID:tool"));
        assert!(!is_adapter_id("hosted:echo:"));
    }

    #[test]
    fn parse_hosted_adapter_id_splits_broker_and_tool() {
        assert_eq!(
            parse_hosted_adapter_id("hosted:vam.files:files.get"),
            Some(("vam.files".to_string(), "files.get".to_string()))
        );
        assert_eq!(
            parse_hosted_adapter_id("hosted:echo:echo.ping"),
            Some(("echo".to_string(), "echo.ping".to_string()))
        );
        assert_eq!(parse_hosted_adapter_id("echo"), None);
    }

    #[test]
    fn mcp_adapter_id_round_trip() {
        let id = mcp_adapter_id("linear", "create_issue");
        assert_eq!(id, "mcp:linear:create_issue");
        assert_eq!(
            parse_mcp_adapter_id(&id),
            Some(("linear".to_string(), "create_issue".to_string()))
        );
        assert!(is_adapter_id(&id));
        assert_eq!(
            parse_mcp_adapter_id("mcp:jira:issues:create"),
            Some(("jira".to_string(), "issues:create".to_string()))
        );
        assert!(parse_mcp_adapter_id("mcp:Linear:create_issue").is_none());
        assert!(parse_mcp_adapter_id("mcp:jira:").is_none());
        assert!(parse_mcp_adapter_id("mcp:a.b:tool").is_none());
        assert!(parse_hosted_adapter_id("mcp:linear:create_issue").is_none());
    }

    #[test]
    fn hosted_adapter_id_builder_round_trip() {
        let id = hosted_adapter_id("echo", "echo.ping");
        assert_eq!(id, "hosted:echo:echo.ping");
        assert_eq!(
            parse_hosted_adapter_id(&id),
            Some(("echo".to_string(), "echo.ping".to_string()))
        );
    }

    fn activation(when: &str, mode: ActivationMode) -> PlaybookActivation {
        PlaybookActivation {
            when: when.to_string(),
            examples: Vec::new(),
            mode: mode as i32,
        }
    }

    fn constraints(err: &ValidationError) -> Vec<&str> {
        err.field_violations
            .iter()
            .map(|violation| violation.constraint.as_str())
            .collect()
    }

    #[test]
    fn draft_without_when_validates_but_publish_requires_it() {
        let mut spec = playbook(AGENTIC_TASK, "");
        assert!(spec.validate().is_ok(), "a draft may omit activation");
        let err = spec.validate_for_publish().expect_err("no activation");
        assert_eq!(constraints(&err), vec!["activation_when_required"]);
        assert!(err
            .to_string()
            .contains("playbook.activation.when is required"));
        assert!(err.to_string().contains("publish again"));

        spec.activation = Some(activation("   ", ActivationMode::Ask));
        assert!(spec.validate_for_publish().is_err(), "blank when");

        spec.activation = Some(activation(
            "Use when the user wants a lint.",
            ActivationMode::Ask,
        ));
        assert!(spec.validate_for_publish().is_ok());
    }

    #[test]
    fn activation_limits_are_enforced() {
        let mut spec = playbook(AGENTIC_TASK, "");
        spec.activation = Some(activation(
            &"x".repeat(ACTIVATION_WHEN_MAX_CHARS + 1),
            ActivationMode::Ask,
        ));
        assert!(constraints(&spec.validate().expect_err("long when")).contains(&"max_len"));

        let mut act = activation("Use when linting.", ActivationMode::Unspecified);
        act.examples = vec!["a".to_string(); ACTIVATION_EXAMPLES_MAX + 1];
        spec.activation = Some(act);
        assert!(constraints(&spec.validate().expect_err("many")).contains(&"max_items"));

        let mut act = activation("Use when linting.", ActivationMode::Ask);
        act.examples = vec![String::new(), "y".repeat(ACTIVATION_EXAMPLE_MAX_CHARS + 1)];
        spec.activation = Some(act);
        let err = spec.validate().expect_err("bad examples");
        assert!(constraints(&err).contains(&"non_empty"));
        assert!(constraints(&err).contains(&"max_len"));

        let mut act = activation("", ActivationMode::Ask);
        act.examples = vec!["lint it".to_string()];
        spec.activation = Some(act);
        assert!(spec.validate().is_err(), "examples need a when");

        let mut act = activation("Use when linting.", ActivationMode::Ask);
        act.mode = 99;
        spec.activation = Some(act);
        assert!(constraints(&spec.validate().expect_err("mode")).contains(&"activation_mode"));
    }

    #[test]
    fn auto_activation_is_refused_for_a_mutating_builtin_step() {
        let write = r#"{"id": "w", "kind": "STEP_KIND_TASK", "mode": "STEP_MODE_BOUND",
            "instruction": "Write.", "binding": {"toolName": "write_file", "arguments": {"path": "a"}}}"#;
        let read = r#"{"id": "r", "kind": "STEP_KIND_TASK", "mode": "STEP_MODE_BOUND",
            "instruction": "Read.", "binding": {"toolName": "read_file", "arguments": {"path": "a"}}}"#;
        let mut spec = playbook(write, "");
        spec.activation = Some(activation("Use when writing.", ActivationMode::Auto));
        let err = spec.validate().expect_err("auto + write_file");
        assert_eq!(constraints(&err), vec!["activation_auto_mutating"]);
        spec.activation = Some(activation("Use when writing.", ActivationMode::Ask));
        assert!(spec.validate().is_ok(), "ask is fine with a write step");

        let mut spec = playbook(read, "");
        spec.activation = Some(activation("Use when reading.", ActivationMode::Auto));
        assert!(spec.validate_for_publish().is_ok());
    }

    fn web_manifest(tier: i32, side_effect: i32, destination: i32) -> CapabilityManifest {
        CapabilityManifest {
            broker_id: "vam.web".to_string(),
            trust_tier: tier,
            tools: vec![ToolExport {
                tool_name: "web_fetch".to_string(),
                side_effect,
                required_secrets: vec![],
                destination_policy: destination,
                effect: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn public_web_destination_is_restricted() {
        let public_web = ToolDestination::PublicWeb as i32;
        let read_only = SideEffectClass::ReadOnly as i32;
        // FIRST_PARTY (2) and OPERATOR (3) READ_ONLY exports may use it.
        assert!(web_manifest(2, read_only, public_web).validate().is_ok());
        assert!(web_manifest(3, read_only, public_web).validate().is_ok());
        // Not on a mutating export.
        let err = web_manifest(2, SideEffectClass::MutableExternal as i32, public_web)
            .validate()
            .unwrap_err();
        assert!(err
            .field_violations
            .iter()
            .any(|v| v.constraint == "destination_policy"));
        // Not on THIRD_PARTY (4).
        assert!(web_manifest(4, read_only, public_web).validate().is_err());
        // The allowlist default stays valid everywhere.
        assert!(web_manifest(4, read_only, 0).validate().is_ok());
        // Unknown values are refused.
        assert!(web_manifest(2, read_only, 7).validate().is_err());
    }

    fn effect_manifest(
        create_effect: Option<crate::v1::EffectSpec>,
        find_side_effect: i32,
    ) -> CapabilityManifest {
        CapabilityManifest {
            broker_id: "vam.eff".to_string(),
            trust_tier: TrustTier::FirstParty as i32,
            tools: vec![
                ToolExport {
                    tool_name: "create".to_string(),
                    side_effect: SideEffectClass::MutableExternal as i32,
                    effect: create_effect,
                    ..Default::default()
                },
                ToolExport {
                    tool_name: "find".to_string(),
                    side_effect: find_side_effect,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn verify_spec(tool: &str, found: Option<crate::v1::EffectFound>) -> crate::v1::EffectSpec {
        crate::v1::EffectSpec {
            class: crate::v1::EffectClass::NonIdempotent as i32,
            verify: Some(crate::v1::EffectVerify {
                tool: tool.to_string(),
                args_template: None,
                found,
            }),
        }
    }

    fn found_non_empty() -> Option<crate::v1::EffectFound> {
        Some(crate::v1::EffectFound {
            path: "found".to_string(),
            r#match: Some(crate::v1::effect_found::Match::NonEmpty(true)),
        })
    }

    #[test]
    fn effect_declarations_are_checked() {
        let read = SideEffectClass::ReadOnly as i32;
        let ok = verify_spec("find", found_non_empty());
        assert!(effect_manifest(Some(ok.clone()), read).validate().is_ok());
        assert!(effect_manifest(None, read).validate().is_ok());
        // The verify tool must be read-only, exist, and not be the tool itself.
        assert!(
            effect_manifest(Some(ok), SideEffectClass::MutableExternal as i32)
                .validate()
                .is_err()
        );
        assert!(
            effect_manifest(Some(verify_spec("nope", found_non_empty())), read)
                .validate()
                .is_err()
        );
        assert!(
            effect_manifest(Some(verify_spec("create", found_non_empty())), read)
                .validate()
                .is_err()
        );
        // A verify read needs a found rule with a match.
        assert!(effect_manifest(Some(verify_spec("find", None)), read)
            .validate()
            .is_err());
        let no_match = Some(crate::v1::EffectFound {
            path: "found".to_string(),
            r#match: None,
        });
        assert!(effect_manifest(Some(verify_spec("find", no_match)), read)
            .validate()
            .is_err());
        // READ is only for a read-only tool; an unknown class is refused.
        let mut declared_read = verify_spec("find", found_non_empty());
        declared_read.class = crate::v1::EffectClass::Read as i32;
        assert!(effect_manifest(Some(declared_read), read)
            .validate()
            .is_err());
        let unknown = crate::v1::EffectSpec {
            class: 9,
            verify: None,
        };
        assert!(effect_manifest(Some(unknown), read).validate().is_err());
    }

    fn run_request_manifest(permission: Option<RunRequestPermission>) -> CapabilityManifest {
        CapabilityManifest {
            broker_id: "vam.sched".to_string(),
            trust_tier: TrustTier::FirstParty as i32,
            tools: vec![
                crate::v1::ToolExport {
                    tool_name: "arm".to_string(),
                    ..Default::default()
                },
                crate::v1::ToolExport {
                    tool_name: "other".to_string(),
                    ..Default::default()
                },
            ],
            run_request_permission: permission,
            ..Default::default()
        }
    }

    fn good_permission() -> RunRequestPermission {
        RunRequestPermission {
            grant_tools: vec!["arm".to_string()],
            loop_guarded_tools: vec!["arm".to_string(), "other".to_string()],
            surface: "schedule".to_string(),
            ..Default::default()
        }
    }

    fn violations(manifest: &CapabilityManifest) -> Vec<String> {
        match manifest.validate() {
            Ok(()) => vec![],
            Err(err) => err
                .field_violations
                .iter()
                .map(|v| v.field_path.clone())
                .collect(),
        }
    }

    fn channel_manifest(
        tier: TrustTier,
        channel: crate::v1::NotificationChannel,
    ) -> CapabilityManifest {
        CapabilityManifest {
            broker_id: "vam.notify".to_string(),
            trust_tier: tier as i32,
            notification_channel: Some(channel),
            ..Default::default()
        }
    }

    fn good_channel() -> crate::v1::NotificationChannel {
        crate::v1::NotificationChannel {
            channel: "slack".to_string(),
            surface: "slack".to_string(),
            max_actions_per_minute: 0,
        }
    }

    #[test]
    fn notification_channel_accepts_core_and_first_party() {
        for tier in [TrustTier::Core, TrustTier::FirstParty] {
            assert!(channel_manifest(tier, good_channel()).validate().is_ok());
        }
    }

    #[test]
    fn notification_channel_refuses_operator_and_third_party() {
        for tier in [TrustTier::Operator, TrustTier::ThirdParty] {
            assert!(violations(&channel_manifest(tier, good_channel()))
                .contains(&"notification_channel".to_string()));
        }
    }

    #[test]
    fn notification_channel_rules() {
        let bad = |f: &dyn Fn(&mut crate::v1::NotificationChannel)| {
            let mut c = good_channel();
            f(&mut c);
            violations(&channel_manifest(TrustTier::FirstParty, c))
        };
        assert!(bad(&|c| c.channel = "email".into())
            .contains(&"notification_channel.channel".to_string()));
        assert!(bad(&|c| c.channel.clear()).contains(&"notification_channel.channel".to_string()));
        for surface in [
            "",
            "sl",
            "Slack",
            "slack1",
            "averyveryverylongsurface",
            "cli",
            "app",
            "api",
            "system",
        ] {
            assert!(
                bad(&|c| c.surface = surface.into())
                    .contains(&"notification_channel.surface".to_string()),
                "{surface}"
            );
        }
        assert!(bad(&|c| c.max_actions_per_minute = -1)
            .contains(&"notification_channel.max_actions_per_minute".to_string()));
    }

    #[test]
    fn channel_decision_is_a_closed_set() {
        let offered = ["opt_a", "Ship it"];
        for ok in ["APPROVED", "REJECTED", "SUBMITTED", "opt_a", "Ship it"] {
            assert!(validate_channel_decision(ok, &offered).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "approved",
            "rejected",
            "Submitted",
            "AMENDED",
            "amended",
            "opt_b",
            "APPROVED_ALWAYS",
            "always",
            "ALWAYS_ALLOW",
            "remember",
            "REMEMBER",
            "approve and remember",
        ] {
            assert!(validate_channel_decision(bad, &offered).is_err(), "{bad}");
        }
        // An option id cannot smuggle a remember or always word.
        assert!(validate_channel_decision("always allow", &["always allow"]).is_err());
    }

    #[test]
    fn run_request_permission_accepts_a_good_declaration() {
        assert!(run_request_manifest(Some(good_permission()))
            .validate()
            .is_ok());
        assert!(run_request_manifest(None).validate().is_ok());
    }

    #[test]
    fn run_request_permission_rules() {
        let mut p = good_permission();
        p.grant_tools.clear();
        assert!(violations(&run_request_manifest(Some(p)))
            .contains(&"run_request_permission.grant_tools".to_string()));

        let mut p = good_permission();
        p.grant_tools = vec!["missing".to_string()];
        p.loop_guarded_tools = vec!["missing".to_string()];
        assert!(!violations(&run_request_manifest(Some(p))).is_empty());

        let mut p = good_permission();
        p.loop_guarded_tools = vec![];
        assert!(!violations(&run_request_manifest(Some(p))).is_empty());

        for bad in [
            "ab",
            "Schedule",
            "sched1",
            "abcdefghijklmnopq",
            "",
            "cli",
            "app",
            "api",
            "system",
        ] {
            let mut p = good_permission();
            p.surface = bad.to_string();
            assert!(
                violations(&run_request_manifest(Some(p)))
                    .contains(&"run_request_permission.surface".to_string()),
                "surface {bad:?} should be refused"
            );
        }

        let mut p = good_permission();
        p.max_requests_per_minute = -1;
        assert!(!violations(&run_request_manifest(Some(p))).is_empty());

        let mut m = run_request_manifest(Some(good_permission()));
        m.trust_tier = TrustTier::ThirdParty as i32;
        assert!(violations(&m).contains(&"run_request_permission".to_string()));
    }

    #[test]
    fn run_request_limits_default_when_zero() {
        let p = good_permission();
        assert_eq!(
            effective_run_request_limits(&p),
            (
                DEFAULT_RUN_REQUESTS_PER_MINUTE,
                DEFAULT_RUN_REQUESTS_PER_DAY,
                DEFAULT_MAX_ACTIVE_GRANTS_PER_USER
            )
        );
        assert_eq!(DEFAULT_RUN_REQUESTS_PER_MINUTE, 60);
        assert_eq!(DEFAULT_RUN_REQUESTS_PER_DAY, 1440);
    }
}
