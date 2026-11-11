//! Run-scoped helpers: strict workspace URI parsing and thread identity extraction.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use prost_types::value::Kind;
use prost_types::{Struct, Value};
use vam_spec::v1::{
    AgentSpec, PlaybookSpec, PlaybookStep, ResourceId, ResourceRef, RunSpec, StepKind, StepMode,
};

use crate::error::OrchestratorError;
use crate::workflow::{PlaybookRunPin, SqliteWorkflowStore};

/// Run-input field that carries the host-rendered playbook procedure.
///
/// The guest copies this string into `{{playbook.procedure}}`. It is not a
/// playbook author input.
pub const PLAYBOOK_PROCEDURE_FIELD: &str = "playbook_procedure";
/// Run-input field with the guidance text of an explicitly started guidance-only playbook.
pub const PLAYBOOK_GUIDANCE_FIELD: &str = "playbook_guidance";
/// Run-input object with the validated inputs of a playbook started with `start_playbook`.
/// Templates read it before the flat run inputs.
pub const PLAYBOOK_INPUTS_FIELD: &str = "playbook_inputs";
/// Run-input string with the playbook status block, set on a gate resume.
pub const PLAYBOOK_STATUS_FIELD: &str = "playbook_status";

/// Run inputs the host sends on every app run, whether or not a playbook
/// schema declares them. They are stripped before a playbook's input schema
/// is checked, so a strict schema (`additionalProperties: false`) does not
/// reject the chat message (`prompt`) or the host's model and iteration budget.
const HOST_INPUT_FIELDS: &[&str] = &[
    "prompt",
    "model",
    "model_provider",
    crate::allowed_models::INPUT_MODEL_SOURCE,
    CONTEXT_WINDOW_FIELD,
    "max_iterations",
    "step_timeout_secs",
    "termination",
    vam_spec::harness::INPUT_KEY,
    "system_instructions",
    "agent_role",
    "agent_display_name",
    PLAYBOOK_PROCEDURE_FIELD,
    PLAYBOOK_GUIDANCE_FIELD,
    PLAYBOOK_INPUTS_FIELD,
    PLAYBOOK_STATUS_FIELD,
    crate::playbook_activation::ATTACHED_PLAYBOOKS_FIELD,
];

/// Run-input string with the task model's context window in tokens (host-owned).
pub const CONTEXT_WINDOW_FIELD: &str = "context_window";
/// The model a guest asks for when the run names none.
const DEFAULT_GUEST_MODEL: &str = "mock-model";

/// Context window of `model` in the node's catalog, if the catalog knows it.
pub fn context_window_for(model: &str) -> Option<i32> {
    let catalog = vam_inference::ModelCatalog::new();
    // A gateway id (`anthropic/claude-haiku-5.5`) has the window of the maker's own entry.
    catalog
        .get(model)
        .map(|entry| entry.context_window)
        .or_else(|| {
            catalog
                .gateway_entry(model)
                .map(|entry| entry.context_window)
        })
        .filter(|window| *window > 0)
}

/// Turns per task when the agent leaves `reasoningPolicy.maxIterations` unset.
pub const DEFAULT_MAX_ITERATIONS: i32 = 50;
/// Upper bound stored on the run, even when the agent asks for more.
pub const MAX_ITERATIONS_HARD_CAP: i32 = 200;
/// Seconds allowed for one ReAct round when the agent leaves `stepTimeout` unset.
pub const DEFAULT_STEP_TIMEOUT_SECS: i64 = 300;
/// Active seconds per task when the caller leaves `maxDuration` unset.
pub const DEFAULT_MAX_DURATION_SECS: i64 = 1800;

pub const DEFAULT_THREAD_REF: &str = "main";
pub const DEFAULT_WORKSPACE_ID: &str = "assistant";

fn map_prefix_err(err: vam_storage::Drive9Error) -> OrchestratorError {
    OrchestratorError::InvalidContract(err.to_string())
}

/// Host-prefixed workspace id from a guest-supplied session workspace field.
///
/// Empty guest id defaults to [`DEFAULT_WORKSPACE_ID`]. The caller's own
/// legacy id (`{owner}--{name}`) and agent-scoped id
/// (`{owner}--{agent_key}--{name}`) are accepted as-is. An unprefixed name
/// becomes the legacy id. A foreign prefix is stripped and re-applied so the
/// caller cannot open another user's volume.
pub fn resolve_session_workspace_id(
    owner_user_id: &str,
    guest_id: &str,
) -> Result<String, OrchestratorError> {
    let suffix = if guest_id.trim().is_empty() {
        DEFAULT_WORKSPACE_ID
    } else {
        guest_id
    };
    vam_storage::resolve_owned_workspace_id(owner_user_id, suffix).map_err(map_prefix_err)
}

/// Host-prefixed thread ref. Inherits the same prefix rule as workspace ids.
pub fn resolve_thread_ref(
    owner_user_id: &str,
    spec: &RunSpec,
) -> Result<String, OrchestratorError> {
    let suffix = extract_thread_ref(spec)?;
    vam_storage::apply_owner_prefix(owner_user_id, &suffix).map_err(map_prefix_err)
}

/// The label a Space gets when the request creates it: whatever follows `volume://`, as
/// written. Empty is the default label. Never a lookup key (locked decision 10).
pub fn space_label_from_uri(uri: &str) -> String {
    uri.trim()
        .strip_prefix("volume://")
        .unwrap_or("")
        .trim_matches('/')
        .to_string()
}

/// The label a new thread gets: `threadRef` (or `inputs.threadRef`) as written; empty when the
/// caller gave none. Never a lookup key.
pub fn thread_label_from_spec(spec: &RunSpec) -> String {
    if !spec.thread_ref.trim().is_empty() {
        return spec.thread_ref.clone();
    }
    spec.inputs
        .as_ref()
        .and_then(|i| i.fields.get("threadRef"))
        .and_then(|v| match v.kind.as_ref() {
            Some(prost_types::value::Kind::StringValue(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Parses `workspaceRefUri` strictly. Only `volume://<id>` is supported in Phase 0.
/// Returns the guest suffix.
pub fn parse_workspace_ref_uri(uri: &str) -> Result<String, OrchestratorError> {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "workspaceRefUri is required (e.g. volume://assistant)".into(),
        ));
    }
    let rest = trimmed.strip_prefix("volume://").ok_or_else(|| {
        OrchestratorError::InvalidContract(format!(
            "unsupported workspaceRefUri scheme (only volume:// is supported): {}",
            trimmed
        ))
    })?;
    let workspace_id = rest.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if workspace_id.is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "workspaceRefUri volume id must not be empty".into(),
        ));
    }
    if workspace_id.contains("..") {
        return Err(OrchestratorError::InvalidContract(
            "workspaceRefUri contains invalid path segments".into(),
        ));
    }
    Ok(workspace_id.to_string())
}

/// Resolves thread identity from RunSpec (proto field or inputs.threadRef fallback).
pub fn extract_thread_ref(spec: &RunSpec) -> Result<String, OrchestratorError> {
    if !spec.thread_ref.trim().is_empty() {
        return vam_storage::vam_dir::sanitize_thread_id(&spec.thread_ref)
            .map_err(|e| OrchestratorError::InvalidContract(e.to_string()));
    }
    if let Some(inputs) = &spec.inputs {
        if let Some(prost_types::value::Kind::StringValue(s)) =
            inputs.fields.get("threadRef").and_then(|v| v.kind.as_ref())
        {
            if !s.trim().is_empty() {
                return vam_storage::vam_dir::sanitize_thread_id(s)
                    .map_err(|e| OrchestratorError::InvalidContract(e.to_string()));
            }
        }
    }
    Ok(DEFAULT_THREAD_REF.to_string())
}

pub fn extract_model(spec: &RunSpec) -> String {
    spec.inputs
        .as_ref()
        .and_then(string_from_inputs_field("model"))
        .unwrap_or_default()
}

/// Returns inline system instructions from an AgentSpec when present.
pub fn extract_system_instructions(agent: &AgentSpec) -> Option<String> {
    match &agent.instruction_source {
        Some(vam_spec::v1::agent_spec::InstructionSource::SystemInstructions(s))
            if !s.is_empty() =>
        {
            Some(s.clone())
        }
        _ => None,
    }
}

/// Injects AgentSpec fields into task inputs when run.json did not supply them.
pub fn merge_agent_into_inputs(
    inputs: Option<prost_types::Struct>,
    agent: &AgentSpec,
) -> Option<prost_types::Struct> {
    let instructions = extract_system_instructions(agent);
    let has_role = agent.role.trim().is_empty();
    let has_display = agent.display_name.trim().is_empty();
    let has_models = agent.preferred_models.is_empty();

    if instructions.is_none() && has_role && has_display && has_models {
        return inputs;
    }

    let mut merged = inputs.unwrap_or_default();

    if let Some(s) = instructions {
        if !merged.fields.contains_key("system_instructions") {
            merged.fields.insert(
                "system_instructions".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(s)),
                },
            );
        }
    }

    if !has_role && !merged.fields.contains_key("agent_role") {
        merged.fields.insert(
            "agent_role".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(agent.role.clone())),
            },
        );
    }

    if !has_display && !merged.fields.contains_key("agent_display_name") {
        merged.fields.insert(
            "agent_display_name".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    agent.display_name.clone(),
                )),
            },
        );
    }

    if !merged.fields.contains_key("model") {
        if let Some(pref) = agent.preferred_models.first() {
            if !pref.model_name.trim().is_empty() {
                merged.fields.insert(
                    "model".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue(
                            pref.model_name.clone(),
                        )),
                    },
                );
            }
            if !pref.provider.trim().is_empty() && !merged.fields.contains_key("model_provider") {
                merged.fields.insert(
                    "model_provider".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue(pref.provider.clone())),
                    },
                );
            }
        }
    }

    // Carry the declared provider alongside the model so the guest does not
    // have to guess it from the model id.
    if !merged.fields.contains_key("model_provider") {
        let model = string_from_struct_field(&merged, "model");
        let provider = model.and_then(|m| {
            agent
                .preferred_models
                .iter()
                .find(|p| p.model_name == m && !p.provider.trim().is_empty())
                .map(|p| p.provider.clone())
        });
        if let Some(provider) = provider {
            merged.fields.insert(
                "model_provider".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(provider)),
                },
            );
        }
    }

    Some(merged)
}

/// Resolve iteration, step, and active-time budgets once and write them onto `spec`.
///
/// `create_run` calls this before the spec is persisted. Dispatch, resume, and
/// active-time expiry read the stored values and do not resolve again.
///
/// `max_iterations` comes from `reasoningPolicy.maxIterations` when that value
/// is at least 1, otherwise [`DEFAULT_MAX_ITERATIONS`], then clamped to
/// [`MAX_ITERATIONS_HARD_CAP`]. A caller `inputs.max_iterations` (string or
/// number) may lower the result. `step_timeout_secs` comes from
/// `reasoningPolicy.stepTimeout` when its seconds are positive, otherwise
/// [`DEFAULT_STEP_TIMEOUT_SECS`]; a caller value is replaced. A positive
/// `maxDuration` is kept; otherwise the run gets [`DEFAULT_MAX_DURATION_SECS`].
/// `termination` is written as `explicit_claim` or `output_satisfied`, and
/// removed when the policy is unset.
pub fn resolve_run_budgets(spec: &mut RunSpec, agent: &AgentSpec) {
    let from_policy = agent
        .reasoning_policy
        .as_ref()
        .map(|policy| policy.max_iterations)
        .filter(|iterations| *iterations >= 1)
        .unwrap_or(DEFAULT_MAX_ITERATIONS);
    let resolved = from_policy.min(MAX_ITERATIONS_HARD_CAP);
    let max_iterations = spec
        .inputs
        .as_ref()
        .and_then(caller_max_iterations)
        .filter(|requested| *requested < resolved)
        .unwrap_or(resolved);

    let step_timeout_secs = agent
        .reasoning_policy
        .as_ref()
        .and_then(|policy| policy.step_timeout.as_ref())
        .filter(|timeout| timeout.seconds > 0)
        .map(|timeout| timeout.seconds)
        .unwrap_or(DEFAULT_STEP_TIMEOUT_SECS);

    let mut inputs = spec.inputs.take().unwrap_or_default();
    insert_string_input(&mut inputs, "max_iterations", &max_iterations.to_string());
    insert_string_input(
        &mut inputs,
        "step_timeout_secs",
        &step_timeout_secs.to_string(),
    );
    // `reasoningPolicy.termination` is host-owned: a caller value is replaced, and
    // an unset policy removes it. The guest reads it from the stored spec.
    match agent
        .reasoning_policy
        .as_ref()
        .map(|policy| policy.termination)
        .and_then(|value| vam_spec::v1::TerminationPolicy::try_from(value).ok())
    {
        Some(vam_spec::v1::TerminationPolicy::ExplicitClaim) => {
            insert_string_input(&mut inputs, "termination", "explicit_claim");
        }
        Some(vam_spec::v1::TerminationPolicy::OutputSatisfied) => {
            insert_string_input(&mut inputs, "termination", "output_satisfied");
        }
        _ => {
            inputs.fields.remove("termination");
        }
    }
    // The model's context window, from the host's catalog: the guest decides
    // when to compact from it. Host-owned; a caller value is replaced.
    let model = string_from_struct_field(&inputs, "model")
        .unwrap_or_else(|| DEFAULT_GUEST_MODEL.to_string());
    match context_window_for(&model) {
        Some(window) => insert_string_input(&mut inputs, CONTEXT_WINDOW_FIELD, &window.to_string()),
        None => {
            inputs.fields.remove(CONTEXT_WINDOW_FIELD);
        }
    }
    spec.inputs = Some(inputs);

    let caller_duration_positive = spec
        .max_duration
        .as_ref()
        .is_some_and(|duration| duration.seconds > 0);
    if !caller_duration_positive {
        spec.max_duration = Some(prost_types::Duration {
            seconds: DEFAULT_MAX_DURATION_SECS,
            nanos: 0,
        });
    }
}

/// The catalog profile of `model` that a run may take on its own: the entry's profile (a
/// gateway id resolves through its maker's entry) unless that profile's evidence is
/// `pending` (a pending profile is never selected implicitly). `None` means `base`.
pub fn catalog_profile_for(model: &str) -> Option<String> {
    let catalog = vam_inference::ModelCatalog::new();
    let entry = catalog
        .get(model)
        .cloned()
        .or_else(|| catalog.gateway_entry(model))?;
    implicit_profile(entry.profile.as_deref())
}

/// `profile` when it names a harness profile whose evidence is not `pending`.
pub fn implicit_profile(profile: Option<&str>) -> Option<String> {
    let found = vam_spec::harness::profile(profile?)?;
    (found.evidence != "pending").then(|| found.name.to_string())
}

/// Writes `profile` as the run's `inputs.harness.profile` unless the caller named one
/// (the caller's profile always wins). `None` changes nothing.
pub fn default_harness_profile(spec: &mut RunSpec, profile: Option<&str>) {
    let Some(profile) = profile else { return };
    let mut inputs = spec.inputs.take().unwrap_or_default();
    let entry = inputs
        .fields
        .entry(vam_spec::harness::INPUT_KEY.to_string())
        .or_insert_with(|| Value {
            kind: Some(Kind::StructValue(Struct::default())),
        });
    if let Some(Kind::StructValue(h)) = entry.kind.as_mut() {
        h.fields
            .entry("profile".to_string())
            .or_insert_with(|| Value {
                kind: Some(Kind::StringValue(profile.to_string())),
            });
    }
    spec.inputs = Some(inputs);
}

/// Resolve the run's `harness` input once and write the host record onto `spec`.
///
/// A caller `inputs.harness` is read as a partial object of overrides to the
/// profile it names (`profile`), or to `base` when it names none: a profile whose
/// evidence is pending is never applied implicitly. An unknown name is refused,
/// naming `profile`. The stored value is the host record from `vam_spec::harness`
/// (`profile`, `version`, `digest`, `overrides`, `switches`). A caller cannot
/// send a record: `version`, `digest` and `switches` are unknown switches and
/// the run is refused, naming the switch. A run with no harness input gets the
/// `base` record, so every receipt names its profile.
///
/// `create_run` calls this before the spec is persisted; resume reads the stored
/// record and never resolves again.
pub fn resolve_harness(spec: &mut RunSpec) -> Result<(), OrchestratorError> {
    let mut inputs = spec.inputs.take().unwrap_or_default();
    let refuse = |spec: &mut RunSpec, inputs: Struct, msg: String| {
        spec.inputs = Some(inputs);
        Err(OrchestratorError::InvalidContract(msg))
    };
    let overrides = match inputs
        .fields
        .get(vam_spec::harness::INPUT_KEY)
        .and_then(|v| v.kind.as_ref())
    {
        Some(Kind::StructValue(given)) => vam_spec::harness::struct_to_json(given),
        Some(Kind::NullValue(_)) | None => serde_json::Value::Object(Default::default()),
        Some(_) => {
            return refuse(
                spec,
                inputs,
                vam_spec::harness::HarnessError::NotObject.to_string(),
            )
        }
    };
    let record = vam_spec::harness::Harness::resolve(&overrides)
        .and_then(|resolved| vam_spec::harness::Harness::record(&resolved));
    let stored = match record {
        Ok(record) => vam_spec::harness::json_to_struct(&record),
        Err(e) => return refuse(spec, inputs, e.to_string()),
    };
    let Some(stored) = stored else {
        return refuse(spec, inputs, "harness record is not an object".into());
    };
    inputs.fields.insert(
        vam_spec::harness::INPUT_KEY.to_string(),
        Value {
            kind: Some(Kind::StructValue(stored)),
        },
    );
    spec.inputs = Some(inputs);
    Ok(())
}

fn caller_max_iterations(inputs: &prost_types::Struct) -> Option<i32> {
    inputs
        .fields
        .get("max_iterations")
        .and_then(positive_i32_from_value)
}

fn positive_i32_from_value(value: &prost_types::Value) -> Option<i32> {
    match value.kind.as_ref()? {
        Kind::NumberValue(number) => i32_from_positive_number(*number),
        Kind::StringValue(text) => text.trim().parse::<i32>().ok().filter(|n| *n >= 1),
        Kind::StructValue(nested) => nested
            .fields
            .get("stringValue")
            .and_then(positive_i32_from_value)
            .or_else(|| {
                nested
                    .fields
                    .get("numberValue")
                    .and_then(positive_i32_from_value)
            }),
        _ => None,
    }
}

fn i32_from_positive_number(number: f64) -> Option<i32> {
    if !number.is_finite() || number < 1.0 {
        return None;
    }
    let truncated = number.trunc();
    if truncated > i32::MAX as f64 {
        return None;
    }
    Some(truncated as i32)
}

fn insert_string_input(inputs: &mut prost_types::Struct, key: &str, value: &str) {
    inputs.fields.insert(
        key.to_string(),
        Value {
            kind: Some(Kind::StringValue(value.to_string())),
        },
    );
}

pub fn extract_prompt_from_inputs(spec: &RunSpec) -> String {
    spec.inputs
        .as_ref()
        .and_then(string_from_inputs_field("prompt"))
        .unwrap_or_else(|| "Execute the assigned run task.".to_string())
}

fn string_from_inputs_field(key: &str) -> impl Fn(&prost_types::Struct) -> Option<String> + '_ {
    move |inputs| {
        string_from_struct_field(inputs, key).or_else(|| {
            inputs.fields.get("fields").and_then(|v| {
                if let prost_types::value::Kind::StructValue(nested) = v.kind.as_ref()? {
                    string_from_struct_field(nested, key)
                } else {
                    None
                }
            })
        })
    }
}

fn string_from_struct_field(s: &prost_types::Struct, key: &str) -> Option<String> {
    s.fields.get(key).and_then(string_from_value)
}

fn string_from_value(v: &prost_types::Value) -> Option<String> {
    match v.kind.as_ref()? {
        prost_types::value::Kind::StringValue(s) => Some(s.clone()),
        prost_types::value::Kind::StructValue(nested) => {
            nested.fields.get("stringValue").and_then(string_from_value)
        }
        _ => None,
    }
}

pub fn agent_digest_hex(agent: &AgentSpec, spec: &RunSpec) -> String {
    if let Some(agent_ref) = spec.agent_ref.as_ref() {
        if let Some(digest) = &agent_ref.digest {
            return digest.hex_hash.clone();
        }
    }
    let _ = agent;
    sha256_empty()
}

fn sha256_empty() -> String {
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string()
}

pub fn agent_urn(agent: &AgentSpec) -> String {
    agent
        .resource_id
        .as_ref()
        .map(|r| r.urn.clone())
        .unwrap_or_default()
}

pub fn requested_agent_urn(spec: &RunSpec) -> Option<String> {
    spec.agent_ref
        .as_ref()
        .and_then(|r| r.resource_id.as_ref())
        .map(|r| r.urn.clone())
        .filter(|u| !u.is_empty())
}

/// When `CreateRun` carries an inline playbook and no playbook ref, pin the
/// inline resource id so catalog resolution can load it.
pub fn ensure_playbook_ref(spec: &mut RunSpec, inline: Option<&PlaybookSpec>) {
    let has_urn = spec
        .playbook_ref
        .as_ref()
        .and_then(|resource_ref| resource_ref.resource_id.as_ref())
        .is_some_and(|id| !id.urn.is_empty());
    if has_urn {
        return;
    }
    let Some(urn) = inline
        .and_then(|playbook| playbook.resource_id.as_ref())
        .map(|id| id.urn.clone())
        .filter(|urn| !urn.is_empty())
    else {
        return;
    };
    spec.playbook_ref = Some(ResourceRef {
        resource_id: Some(ResourceId { urn }),
        ..Default::default()
    });
}

/// Pinned playbook loaded for a run, plus the procedure text for the guest.
#[derive(Debug, Clone)]
pub struct PreparedPlaybookAdmission {
    pub pin: PlaybookRunPin,
    pub spec: PlaybookSpec,
    /// The rendered procedure, or the guidance text of a guidance-only playbook.
    pub procedure: String,
}

impl PreparedPlaybookAdmission {
    /// A guidance-only playbook: no steps, nothing tracked.
    pub fn is_guidance_only(&self) -> bool {
        self.spec.steps.is_empty()
    }
}

/// Load the pinned playbook, check `inputs` against `input_schema`, and render
/// the procedure. Does not write step state; the run row does not need to exist.
///
/// `None` means the run has no playbook. Host-owned input fields (`prompt`,
/// `model`, `max_iterations`, `step_timeout_secs`, agent context, the procedure
/// itself) are ignored when the schema does not declare them.
pub fn prepare_playbook_admission(
    store: &SqliteWorkflowStore,
    spec: &RunSpec,
    owner_user_id: &str,
    include_foreign_drafts: bool,
) -> Result<Option<PreparedPlaybookAdmission>, OrchestratorError> {
    let Some(playbook_ref) = spec.playbook_ref.as_ref() else {
        return Ok(None);
    };
    prepare_playbook_for_ref(
        store,
        playbook_ref,
        spec.inputs.as_ref(),
        owner_user_id,
        include_foreign_drafts,
    )
    .map(Some)
}

/// The admission core: load the pinned playbook at `playbook_ref`, check
/// `inputs` against its `input_schema`, and render the procedure. Used at run
/// creation and by `start_playbook` when a playbook is bound mid-run.
pub fn prepare_playbook_for_ref(
    store: &SqliteWorkflowStore,
    playbook_ref: &ResourceRef,
    inputs: Option<&Struct>,
    owner_user_id: &str,
    include_foreign_drafts: bool,
) -> Result<PreparedPlaybookAdmission, OrchestratorError> {
    let urn = playbook_ref
        .resource_id
        .as_ref()
        .map(|id| id.urn.as_str())
        .filter(|urn| !urn.is_empty())
        .ok_or_else(|| {
            OrchestratorError::InvalidContract("playbook ref is missing a URN".to_string())
        })?;
    let version = playbook_ref.version.as_ref().ok_or_else(|| {
        OrchestratorError::InvalidContract(format!("playbook '{urn}' is not pinned to a version"))
    })?;
    let digest = playbook_ref
        .digest
        .as_ref()
        .map(|digest| digest.hex_hash.as_str())
        .filter(|hex| !hex.is_empty())
        .ok_or_else(|| {
            OrchestratorError::InvalidContract(format!(
                "playbook '{urn}' is not pinned to a digest"
            ))
        })?;
    let mut playbook =
        store.get_playbook_at_version(urn, version, owner_user_id, include_foreign_drafts)?;
    vam_spec::playbook::migrate_playbook_spec(&mut playbook);
    let checked = inputs_for_schema_check(playbook.input_schema.as_ref(), inputs);
    vam_spec::validate_playbook_inputs(playbook.input_schema.as_ref(), checked.as_ref())
        .map_err(|err| OrchestratorError::InvalidContract(err.to_string()))?;
    let procedure = if playbook.steps.is_empty() {
        playbook.guidance.trim().to_string()
    } else {
        render_playbook_procedure(&playbook, inputs, &[])
    };
    Ok(PreparedPlaybookAdmission {
        pin: PlaybookRunPin {
            urn: urn.to_string(),
            version: format_version(version),
            digest: digest.to_string(),
        },
        spec: playbook,
        procedure,
    })
}

/// Persist step state for a playbook run. The run row must already exist.
pub fn commit_playbook_admission(
    store: &SqliteWorkflowStore,
    run_id: &str,
    prepared: &PreparedPlaybookAdmission,
) -> Result<(), OrchestratorError> {
    if prepared.is_guidance_only() {
        return Ok(());
    }
    store.init_playbook_run(run_id, &prepared.pin, &prepared.spec)?;
    Ok(())
}

/// Put what the guest needs on the run inputs: the procedure of a step
/// playbook, or the guidance text of a guidance-only one.
pub fn insert_playbook_context(
    inputs: Option<Struct>,
    prepared: &PreparedPlaybookAdmission,
) -> Struct {
    if !prepared.is_guidance_only() {
        return insert_playbook_procedure(inputs, &prepared.procedure);
    }
    let mut merged = inputs.unwrap_or_default();
    merged.fields.insert(
        PLAYBOOK_GUIDANCE_FIELD.to_string(),
        Value {
            kind: Some(Kind::StringValue(prepared.procedure.clone())),
        },
    );
    merged
}

/// Store the rendered procedure on run inputs for the guest prompt.
pub fn insert_playbook_procedure(inputs: Option<Struct>, procedure: &str) -> Struct {
    let mut merged = inputs.unwrap_or_default();
    merged.fields.insert(
        PLAYBOOK_PROCEDURE_FIELD.to_string(),
        Value {
            kind: Some(Kind::StringValue(procedure.to_string())),
        },
    );
    merged
}

/// Render playbook prose with `{{inputs.<path>}}` and `{{steps.<id>.output.<path>}}`.
///
/// `step_outputs_json` is `(step id, JSON object)`. A path that cannot be
/// resolved is left as the original token. At admission, step outputs are empty.
pub fn render_playbook_procedure(
    playbook: &PlaybookSpec,
    inputs: Option<&Struct>,
    step_outputs_json: &[(&str, &str)],
) -> String {
    let inputs_json = template_inputs_json(inputs);
    let mut steps = BTreeMap::<String, serde_json::Value>::new();
    for (id, json) in step_outputs_json {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(json) {
            steps.insert((*id).to_string(), value);
        }
    }

    let title = {
        let trimmed = playbook.display_name.trim();
        if trimmed.is_empty() {
            "Playbook".to_string()
        } else {
            trimmed.to_string()
        }
    };
    let mut lines = Vec::new();
    lines.push(format!("# {title}"));
    if !playbook.description.trim().is_empty() {
        lines.push(String::new());
        lines.push(playbook.description.trim().to_string());
    }
    lines.push(String::new());
    lines.push(
        "Complete available steps in any order. The host tracks each step. Each step names the tool that completes it."
            .to_string(),
    );
    lines.push(vam_spec::playbook::PLAYBOOK_GATE_RULE.to_string());
    for step in &playbook.steps {
        lines.push(String::new());
        lines.push(format!("## {}", step.id));
        lines.push(format!("kind: {}", step_kind_name(step.kind)));
        if let Some(mode) = task_mode_name(step) {
            lines.push(format!("mode: {mode}"));
        }
        lines.push(crate::workflow::playbook_step_completion_line(step));
        if !step.after.is_empty() {
            lines.push(format!("after: {}", step.after.join(", ")));
        }
        if !step.after_any.is_empty() {
            lines.push(format!("after_any: {}", step.after_any.join(", ")));
        }
        let outcomes = outcome_labels(step);
        if !outcomes.is_empty() {
            lines.push(format!("outcomes: {}", outcomes.join(", ")));
        }
        let instruction = render_templates(&step.instruction, &inputs_json, &steps);
        if !instruction.trim().is_empty() {
            lines.push(instruction);
        }
        let question = render_templates(&step.question, &inputs_json, &steps);
        if !question.trim().is_empty() {
            lines.push(question);
        }
    }
    lines.join("\n")
}

fn inputs_for_schema_check(schema: Option<&Struct>, inputs: Option<&Struct>) -> Option<Struct> {
    let inputs = inputs?;
    let declared = schema_property_names(schema);
    let mut filtered = inputs.clone();
    for key in HOST_INPUT_FIELDS {
        if !declared.contains(*key) {
            filtered.fields.remove(*key);
        }
    }
    Some(filtered)
}

fn schema_property_names(schema: Option<&Struct>) -> HashSet<String> {
    let Some(schema) = schema else {
        return HashSet::new();
    };
    let json = struct_to_json(schema);
    json.get("properties")
        .and_then(|value| value.as_object())
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default()
}

fn format_version(version: &vam_spec::v1::Version) -> String {
    let mut out = format!("{}.{}.{}", version.major, version.minor, version.patch);
    if !version.prerelease.is_empty() {
        out.push('-');
        out.push_str(&version.prerelease);
    }
    if !version.build.is_empty() {
        out.push('+');
        out.push_str(&version.build);
    }
    out
}

fn step_kind_name(kind: i32) -> &'static str {
    match StepKind::try_from(kind) {
        Ok(StepKind::Gate) => "gate",
        _ => "task",
    }
}

/// Task steps name who supplies the tool. Gates have no mode line.
fn task_mode_name(step: &PlaybookStep) -> Option<&'static str> {
    if step_kind_name(step.kind) == "gate" {
        return None;
    }
    Some(
        match StepMode::try_from(step.mode).unwrap_or(StepMode::Unspecified) {
            StepMode::Bound => "bound",
            StepMode::Assisted => "assisted",
            StepMode::Agentic => "agentic",
            StepMode::Unspecified => "unspecified",
        },
    )
}

fn outcome_labels(step: &PlaybookStep) -> Vec<String> {
    let mut labels = Vec::new();
    for outcome in step.outcomes.iter().chain(step.options.iter()) {
        if outcome.label.is_empty() || labels.iter().any(|existing| existing == &outcome.label) {
            continue;
        }
        labels.push(outcome.label.clone());
    }
    if labels.is_empty() && step_kind_name(step.kind) == "task" {
        labels.push("done".to_string());
    }
    labels
}

fn render_templates(
    text: &str,
    inputs: &serde_json::Value,
    steps: &BTreeMap<String, serde_json::Value>,
) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        let Some(start) = rest.find("{{") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            break;
        };
        let body = after[..end].trim();
        let token = &rest[start..start + 2 + end + 2];
        match render_template_body(body, inputs, steps) {
            Some(value) => out.push_str(&value),
            None => out.push_str(token),
        }
        rest = &after[end + 2..];
    }
    out
}

fn render_template_body(
    body: &str,
    inputs: &serde_json::Value,
    steps: &BTreeMap<String, serde_json::Value>,
) -> Option<String> {
    if let Some(suffix) = body.strip_prefix("inputs") {
        if !suffix.is_empty() && !suffix.starts_with('.') && !suffix.starts_with('[') {
            return None;
        }
        return resolve_path(inputs, suffix);
    }
    let rest = body.strip_prefix("steps.")?;
    let output_at = rest.find(".output")?;
    let id = &rest[..output_at];
    if !step_id_chars_ok(id) {
        return None;
    }
    let suffix = &rest[output_at + ".output".len()..];
    if !suffix.is_empty() && !suffix.starts_with('.') && !suffix.starts_with('[') {
        return None;
    }
    let output = steps.get(id)?;
    resolve_path(output, suffix)
}

fn step_id_chars_ok(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn resolve_path(value: &serde_json::Value, path: &str) -> Option<String> {
    let values = resolve_values(value, path)?;
    Some(
        values
            .iter()
            .map(format_scalar)
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn resolve_values(value: &serde_json::Value, path: &str) -> Option<Vec<serde_json::Value>> {
    if path.is_empty() {
        return Some(vec![value.clone()]);
    }
    if let Some(rest) = path.strip_prefix("[*]") {
        let items = value.as_array()?;
        let mut out = Vec::new();
        for item in items {
            out.extend(resolve_values(item, rest)?);
        }
        return Some(out);
    }
    if let Some(rest) = path.strip_prefix('[') {
        let (digits, after) = rest.split_once(']')?;
        if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
            return None;
        }
        let index: usize = digits.parse().ok()?;
        let item = value.as_array()?.get(index)?;
        return resolve_values(item, after);
    }
    if let Some(rest) = path.strip_prefix('.') {
        let end = rest
            .find(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        let child = value.as_object()?.get(&rest[..end])?;
        return resolve_values(child, &rest[end..]);
    }
    None
}

fn format_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                integer.to_string()
            } else if let Some(integer) = number.as_u64() {
                integer.to_string()
            } else if let Some(float) = number.as_f64() {
                if float.is_finite() && float.fract() == 0.0 {
                    format!("{float:.0}")
                } else {
                    float.to_string()
                }
            } else {
                number.to_string()
            }
        }
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn struct_json(value: &Struct) -> serde_json::Value {
    struct_to_json(value)
}

/// Render a gate question with the same `{{inputs}}` / `{{steps}}` rules as the procedure.
pub(crate) fn render_gate_question(
    question: &str,
    inputs: Option<&Struct>,
    step_outputs_json: &[(&str, &str)],
) -> String {
    let inputs_json = template_inputs_json(inputs);
    let mut steps = BTreeMap::<String, serde_json::Value>::new();
    for (id, json) in step_outputs_json {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(json) {
            steps.insert((*id).to_string(), value);
        }
    }
    render_templates(question, &inputs_json, &steps)
}

/// Values at `path` (`.field`, `[n]`, `[*]`) inside a step output object.
///
/// A single array is returned as its elements so `choices_from` can fill an enum.
pub(crate) fn choice_values_from_output(output_json: &str, path: &str) -> Vec<serde_json::Value> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output_json) else {
        return Vec::new();
    };
    let Some(values) = resolve_values(&value, path) else {
        return Vec::new();
    };
    if values.len() == 1 {
        if let Some(items) = values[0].as_array() {
            return items.clone();
        }
    }
    values
}

// ---------------------------------------------------------------------------
// Playbooks in flight (decision 21): template inputs, bound-argument
// resolution with provenance, gate defaults, and the status block.
// ---------------------------------------------------------------------------

/// Inputs that `{{inputs.<path>}}` reads: the validated `playbook_inputs` of a
/// playbook started with `start_playbook`, else the run's own inputs.
pub(crate) fn template_inputs_json(inputs: Option<&Struct>) -> serde_json::Value {
    let Some(inputs) = inputs else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    if let Some(Value {
        kind: Some(Kind::StructValue(nested)),
    }) = inputs.fields.get(PLAYBOOK_INPUTS_FIELD)
    {
        return struct_to_json(nested);
    }
    struct_to_json(inputs)
}

/// Step outputs by step id, parsed from the stored JSON.
pub(crate) fn step_outputs_map(
    records: &[crate::workflow::PlaybookStepRecord],
) -> BTreeMap<String, serde_json::Value> {
    records
        .iter()
        .filter_map(|record| {
            let json = record.output_json.as_deref()?;
            let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
            Some((record.step_id.clone(), value))
        })
        .collect()
}

/// Where a resolved template value came from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum TemplateSource {
    /// The playbook's validated start inputs.
    Inputs,
    /// `steps.<id>.output`.
    Step(String),
}

/// Bound-step arguments with every template resolved, and where the values came from.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedArguments {
    pub arguments: Struct,
    pub sources: BTreeSet<TemplateSource>,
}

/// Resolve `{{inputs.…}}` and `{{steps.<id>.output.…}}` in `arguments`.
///
/// A string that is exactly one template takes the referenced value with its
/// type (number, array, object). Otherwise templates are interpolated as text.
/// A template that cannot be resolved is an error naming it; the literal token
/// is never passed to a tool.
pub fn resolve_binding_arguments(
    arguments: Option<&Struct>,
    inputs: &serde_json::Value,
    steps: &BTreeMap<String, serde_json::Value>,
) -> Result<ResolvedArguments, String> {
    let Some(arguments) = arguments else {
        return Ok(ResolvedArguments {
            arguments: Struct::default(),
            sources: BTreeSet::new(),
        });
    };
    let mut sources = BTreeSet::new();
    let resolved = resolve_json_templates(&struct_to_json(arguments), inputs, steps, &mut sources)?;
    let arguments = crate::playbook_activation::json_to_struct(&resolved)
        .ok_or_else(|| "resolved arguments are not an object".to_string())?;
    Ok(ResolvedArguments { arguments, sources })
}

fn resolve_json_templates(
    value: &serde_json::Value,
    inputs: &serde_json::Value,
    steps: &BTreeMap<String, serde_json::Value>,
    sources: &mut BTreeSet<TemplateSource>,
) -> Result<serde_json::Value, String> {
    match value {
        serde_json::Value::String(text) => resolve_text_templates(text, inputs, steps, sources),
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| resolve_json_templates(item, inputs, steps, sources))
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, item) in map {
                out.insert(
                    key.clone(),
                    resolve_json_templates(item, inputs, steps, sources)?,
                );
            }
            Ok(serde_json::Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

fn resolve_text_templates(
    text: &str,
    inputs: &serde_json::Value,
    steps: &BTreeMap<String, serde_json::Value>,
    sources: &mut BTreeSet<TemplateSource>,
) -> Result<serde_json::Value, String> {
    if !text.contains("{{") {
        return Ok(serde_json::Value::String(text.to_string()));
    }
    let trimmed = text.trim();
    if trimmed.starts_with("{{")
        && trimmed.ends_with("}}")
        && trimmed.matches("{{").count() == 1
        && trimmed.matches("}}").count() == 1
    {
        let body = trimmed[2..trimmed.len() - 2].trim();
        return lookup_template(body, inputs, steps, sources);
    }
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| format!("unterminated template in '{text}'"))?;
        let value = lookup_template(after[..end].trim(), inputs, steps, sources)?;
        out.push_str(&value_as_text(&value));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(serde_json::Value::String(out))
}

fn value_as_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Array(items) => items
            .iter()
            .map(format_scalar)
            .collect::<Vec<_>>()
            .join(", "),
        other => format_scalar(other),
    }
}

fn lookup_template(
    body: &str,
    inputs: &serde_json::Value,
    steps: &BTreeMap<String, serde_json::Value>,
    sources: &mut BTreeSet<TemplateSource>,
) -> Result<serde_json::Value, String> {
    let unresolved = |why: &str| format!("cannot resolve {{{{{body}}}}}: {why}");
    let (source, root, suffix): (TemplateSource, &serde_json::Value, &str) =
        if let Some(suffix) = body.strip_prefix("inputs") {
            if !suffix.is_empty() && !suffix.starts_with('.') && !suffix.starts_with('[') {
                return Err(unresolved("not an inputs path"));
            }
            (TemplateSource::Inputs, inputs, suffix)
        } else if let Some(rest) = body.strip_prefix("steps.") {
            let output_at = rest
                .find(".output")
                .ok_or_else(|| unresolved("not a step output path"))?;
            let id = &rest[..output_at];
            if !step_id_chars_ok(id) {
                return Err(unresolved("bad step id"));
            }
            let suffix = &rest[output_at + ".output".len()..];
            if !suffix.is_empty() && !suffix.starts_with('.') && !suffix.starts_with('[') {
                return Err(unresolved("not a step output path"));
            }
            let output = steps
                .get(id)
                .ok_or_else(|| unresolved(&format!("step '{id}' has no output yet")))?;
            (TemplateSource::Step(id.to_string()), output, suffix)
        } else {
            return Err(unresolved(
                "templates start with inputs or steps.<id>.output",
            ));
        };
    let mut values =
        resolve_values(root, suffix).ok_or_else(|| unresolved("no value at that path"))?;
    sources.insert(source);
    if values.len() == 1 {
        Ok(values.remove(0))
    } else {
        Ok(serde_json::Value::Array(values))
    }
}

/// Render templates in a gate `response_schema` before the gate opens (G5).
///
/// `default` takes the referenced value with its type; an unresolved default is
/// dropped, never shown as a token. `title` and `description` are rendered as text.
pub(crate) fn render_schema_templates(
    schema: &mut serde_json::Value,
    inputs: &serde_json::Value,
    steps: &BTreeMap<String, serde_json::Value>,
) {
    match schema {
        serde_json::Value::Object(map) => {
            if let Some(default) = map.remove("default") {
                let mut sources = BTreeSet::new();
                if let Ok(resolved) = resolve_json_templates(&default, inputs, steps, &mut sources)
                {
                    map.insert("default".to_string(), resolved);
                }
            }
            for key in ["title", "description"] {
                if let Some(serde_json::Value::String(text)) = map.get(key) {
                    let rendered = render_templates(text, inputs, steps);
                    map.insert(key.to_string(), serde_json::Value::String(rendered));
                }
            }
            for (key, child) in map.iter_mut() {
                if key != "default" {
                    render_schema_templates(child, inputs, steps);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                render_schema_templates(item, inputs, steps);
            }
        }
        _ => {}
    }
}

/// Most characters of captured values in one status block.
pub const STATUS_VALUES_MAX_CHARS: usize = 3000;
const STATUS_VALUE_MAX_CHARS: usize = 160;
const STATUS_ARRAY_ITEMS: usize = 3;

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn summarize_value(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::Array(items) if items.len() > STATUS_ARRAY_ITEMS => {
            let shown: Vec<String> = items
                .iter()
                .take(STATUS_ARRAY_ITEMS)
                .map(|item| item.to_string())
                .collect();
            format!(
                "[{}, … +{} more]",
                shown.join(", "),
                items.len() - STATUS_ARRAY_ITEMS
            )
        }
        other => other.to_string(),
    };
    clip(&text, STATUS_VALUE_MAX_CHARS)
}

fn captured_lines(id: &str, output: &serde_json::Value) -> Vec<String> {
    match output.as_object() {
        Some(map) if !map.is_empty() => map
            .iter()
            .map(|(key, value)| format!("- {id}.{key} = {}", summarize_value(value)))
            .collect(),
        Some(_) => Vec::new(),
        None => vec![format!("- {id} = {}", summarize_value(output))],
    }
}

/// The playbook status block (G6): steps done, captured values, steps available
/// now, and the gate awaited. It goes after the cached prompt prefix, in a tool
/// result or an appended message, never in the system prompt.
///
/// Values captured from bound steps are tool output, so they sit inside one
/// untrusted-data fence. Captured values are limited to
/// [`STATUS_VALUES_MAX_CHARS`] characters; the oldest are dropped first.
pub fn render_playbook_status(
    spec: &PlaybookSpec,
    records: &[crate::workflow::PlaybookStepRecord],
    inputs: Option<&Struct>,
    nonce: &vam_security::vendor_text::FenceNonce,
) -> String {
    use crate::workflow::PlaybookStepState;
    let inputs_json = template_inputs_json(inputs);
    let outputs = step_outputs_map(records);
    let name = {
        let trimmed = spec.display_name.trim();
        if trimmed.is_empty() {
            "Playbook"
        } else {
            trimmed
        }
    };

    // (from a bound tool, line)
    let mut entries: Vec<(bool, String)> = Vec::new();
    for key in schema_property_names(spec.input_schema.as_ref())
        .into_iter()
        .collect::<BTreeSet<_>>()
    {
        if let Some(value) = inputs_json.get(&key) {
            entries.push((
                false,
                format!("- inputs.{key} = {}", summarize_value(value)),
            ));
        }
    }
    let mut done = Vec::new();
    for step in &spec.steps {
        let Some(record) = records.iter().find(|r| r.step_id == step.id) else {
            continue;
        };
        if record.state != PlaybookStepState::Completed {
            continue;
        }
        done.push(format!("{}={}", step.id, record.outcome));
        if let Some(output) = outputs.get(&step.id) {
            let bound = task_mode_name(step) == Some("bound");
            for line in captured_lines(&step.id, output) {
                entries.push((bound, line));
            }
        }
    }
    let mut total: usize = entries.iter().map(|(_, line)| line.chars().count()).sum();
    let mut omitted = 0usize;
    while total > STATUS_VALUES_MAX_CHARS && !entries.is_empty() {
        let (_, line) = entries.remove(0);
        total -= line.chars().count();
        omitted += 1;
    }

    let mut lines = vec![format!("[Playbook status: {name}]")];
    lines.push(if done.is_empty() {
        "Done: nothing yet".to_string()
    } else {
        format!("Done: {}", done.join(", "))
    });
    let plain: Vec<&str> = entries
        .iter()
        .filter(|(bound, _)| !bound)
        .map(|(_, line)| line.as_str())
        .collect();
    let vendor: Vec<&str> = entries
        .iter()
        .filter(|(bound, _)| *bound)
        .map(|(_, line)| line.as_str())
        .collect();
    if plain.is_empty() && vendor.is_empty() {
        lines.push("Captured: nothing yet".to_string());
    } else {
        lines.push("Captured:".to_string());
        lines.extend(plain.iter().map(|line| line.to_string()));
        if !vendor.is_empty() {
            lines.push("Captured from tools:".to_string());
            lines.push(
                vam_security::vendor_text::fence(
                    "playbook.bound-steps",
                    vam_security::vendor_text::VendorTextKind::ToolResult,
                    nonce,
                    &vendor.join("\n"),
                )
                .text,
            );
        }
    }
    if omitted > 0 {
        lines.push(format!("({omitted} older captured values left out)"));
    }

    let mut available = Vec::new();
    let mut waiting = Vec::new();
    let mut skipped = Vec::new();
    for step in &spec.steps {
        let Some(record) = records.iter().find(|r| r.step_id == step.id) else {
            continue;
        };
        match record.state {
            PlaybookStepState::Available => {
                let hint = match task_mode_name(step) {
                    Some("bound") => "bound: call run_step".to_string(),
                    Some("assisted") => {
                        let tool = step
                            .binding
                            .as_ref()
                            .map(|b| b.tool_name.as_str())
                            .unwrap_or("its tool");
                        format!("assisted: call {tool}, then step_complete")
                    }
                    _ => "agentic: do it, then step_complete".to_string(),
                };
                available.push(format!("{} ({hint})", step.id));
            }
            PlaybookStepState::Open => {
                let question = render_templates(&step.question, &inputs_json, &outputs);
                let options = outcome_labels(step);
                let options = if options.is_empty() {
                    String::new()
                } else {
                    format!(" [options: {}]", options.join(", "))
                };
                waiting.push(format!(
                    "{}: \"{}\"{options}",
                    step.id,
                    clip(question.trim(), STATUS_VALUE_MAX_CHARS)
                ));
            }
            PlaybookStepState::Skipped => skipped.push(step.id.clone()),
            _ => {}
        }
    }
    lines.push(if available.is_empty() {
        "Available now: none".to_string()
    } else {
        format!("Available now: {}", available.join(", "))
    });
    if !waiting.is_empty() {
        lines.push(format!(
            "Waiting for the user (do not ask in chat; the form is open): {}",
            waiting.join("; ")
        ));
    }
    if !skipped.is_empty() {
        lines.push(format!("Skipped: {}", skipped.join(", ")));
    }
    lines.join("\n")
}

fn struct_to_json(value: &Struct) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (key, field) in &value.fields {
        map.insert(key.clone(), value_to_json(field));
    }
    serde_json::Value::Object(map)
}

fn value_to_json(value: &Value) -> serde_json::Value {
    match &value.kind {
        Some(Kind::NullValue(_)) | None => serde_json::Value::Null,
        Some(Kind::NumberValue(number)) => serde_json::Number::from_f64(*number)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(Kind::StringValue(text)) => serde_json::Value::String(text.clone()),
        Some(Kind::BoolValue(flag)) => serde_json::Value::Bool(*flag),
        Some(Kind::StructValue(nested)) => struct_to_json(nested),
        Some(Kind::ListValue(list)) => {
            serde_json::Value::Array(list.values.iter().map(value_to_json).collect())
        }
    }
}

pub fn agent_ref_from_urn(urn: &str, digest: &str) -> vam_spec::v1::ResourceRef {
    vam_spec::v1::ResourceRef {
        resource_id: Some(vam_spec::v1::ResourceId {
            urn: urn.to_string(),
        }),
        version: Some(vam_spec::v1::Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: String::new(),
            build: String::new(),
        }),
        digest: Some(vam_spec::v1::Digest {
            algorithm: 1,
            hex_hash: digest.to_string(),
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_volume_uri() {
        assert_eq!(
            parse_workspace_ref_uri("volume://assistant").unwrap(),
            "assistant"
        );
        assert_eq!(
            parse_workspace_ref_uri("volume://acme/projects/demo").unwrap(),
            "demo"
        );
        assert!(parse_workspace_ref_uri("").is_err());
        assert!(parse_workspace_ref_uri("file:///tmp/ws").is_err());
        assert!(parse_workspace_ref_uri("volume://").is_err());
    }

    #[test]
    fn guest_cannot_set_or_strip_workspace_prefix() {
        assert_eq!(
            resolve_session_workspace_id("bob", "alice--assistant").unwrap(),
            "bob--assistant"
        );
        assert_eq!(
            resolve_session_workspace_id("local", "").unwrap(),
            "local--assistant"
        );
    }

    #[test]
    fn session_workspace_id_accepts_agent_scoped_form() {
        let scoped = "alice--test-agent-149c38b2--main";
        assert_eq!(
            resolve_session_workspace_id("alice", scoped).unwrap(),
            scoped
        );
        assert_eq!(
            resolve_session_workspace_id("alice", "alice--main").unwrap(),
            "alice--main"
        );
        assert_eq!(
            resolve_session_workspace_id("alice", "bob--test-agent-149c38b2--main").unwrap(),
            "alice--main"
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)] // test fixture style
    fn thread_ref_inherits_host_prefix() {
        let spec = RunSpec::default();
        assert_eq!(extract_thread_ref(&spec).unwrap(), "main");
        assert_eq!(resolve_thread_ref("alice", &spec).unwrap(), "alice--main");
        let mut stolen = RunSpec::default();
        stolen.thread_ref = "bob--main".into();
        assert_eq!(resolve_thread_ref("alice", &stolen).unwrap(), "alice--main");
    }

    #[test]
    fn thread_ref_defaults_to_main() {
        let spec = RunSpec::default();
        assert_eq!(extract_thread_ref(&spec).unwrap(), "main");
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)] // test fixture style
    fn thread_ref_from_inputs() {
        let mut spec = RunSpec::default();
        spec.inputs = Some(prost_types::Struct {
            fields: [(
                "threadRef".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue("feature-x".into())),
                },
            )]
            .into_iter()
            .collect(),
        });
        assert_eq!(extract_thread_ref(&spec).unwrap(), "feature-x");
    }

    #[test]
    fn merge_agent_carries_preferred_provider() {
        let agent = AgentSpec {
            preferred_models: vec![vam_spec::v1::ModelPreference {
                provider: "vercel".into(),
                model_name: "openai/gpt-4o-mini".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let merged = merge_agent_into_inputs(None, &agent).expect("expected struct");
        let get = |k: &str| match merged.fields.get(k).and_then(|v| v.kind.as_ref()) {
            Some(prost_types::value::Kind::StringValue(s)) => s.clone(),
            _ => String::new(),
        };
        assert_eq!(get("model"), "openai/gpt-4o-mini");
        assert_eq!(get("model_provider"), "vercel");
    }

    #[test]
    fn merge_agent_injects_role_and_display_name() {
        let agent = AgentSpec {
            display_name: "Bun Assistant".into(),
            role: "Helpful Assistant".into(),
            ..Default::default()
        };
        let merged = merge_agent_into_inputs(None, &agent).expect("expected struct");
        let role = merged
            .fields
            .get("agent_role")
            .and_then(|v| v.kind.as_ref());
        let name = merged
            .fields
            .get("agent_display_name")
            .and_then(|v| v.kind.as_ref());
        assert!(matches!(
            role,
            Some(prost_types::value::Kind::StringValue(s)) if s == "Helpful Assistant"
        ));
        assert!(matches!(
            name,
            Some(prost_types::value::Kind::StringValue(s)) if s == "Bun Assistant"
        ));
    }

    #[test]
    fn merge_agent_preserves_run_json_role_fields() {
        let agent = AgentSpec {
            display_name: "From Agent".into(),
            role: "From Agent Role".into(),
            ..Default::default()
        };
        let inputs = Some(prost_types::Struct {
            fields: [
                (
                    "agent_role".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue(
                            "from run.json".into(),
                        )),
                    },
                ),
                (
                    "agent_display_name".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue(
                            "from run.json name".into(),
                        )),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        });
        let merged = merge_agent_into_inputs(inputs, &agent).expect("expected struct");
        let role = merged
            .fields
            .get("agent_role")
            .and_then(|v| v.kind.as_ref());
        let name = merged
            .fields
            .get("agent_display_name")
            .and_then(|v| v.kind.as_ref());
        assert!(matches!(
            role,
            Some(prost_types::value::Kind::StringValue(s)) if s == "from run.json"
        ));
        assert!(matches!(
            name,
            Some(prost_types::value::Kind::StringValue(s)) if s == "from run.json name"
        ));
    }

    #[test]
    fn merge_agent_injects_system_instructions_when_missing() {
        let agent = AgentSpec {
            instruction_source: Some(
                vam_spec::v1::agent_spec::InstructionSource::SystemInstructions(
                    "Use calculator for math.".into(),
                ),
            ),
            ..Default::default()
        };
        let merged = merge_agent_into_inputs(None, &agent).expect("expected struct");
        let val = merged
            .fields
            .get("system_instructions")
            .and_then(|v| v.kind.as_ref());
        assert!(matches!(
            val,
            Some(prost_types::value::Kind::StringValue(s)) if s == "Use calculator for math."
        ));
    }

    #[test]
    fn merge_agent_preserves_run_json_system_instructions() {
        let agent = AgentSpec {
            instruction_source: Some(
                vam_spec::v1::agent_spec::InstructionSource::SystemInstructions(
                    "from agent spec".into(),
                ),
            ),
            ..Default::default()
        };
        let inputs = Some(prost_types::Struct {
            fields: [(
                "system_instructions".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(
                        "from run.json".into(),
                    )),
                },
            )]
            .into_iter()
            .collect(),
        });
        let merged = merge_agent_into_inputs(inputs, &agent).expect("expected struct");
        let val = merged
            .fields
            .get("system_instructions")
            .and_then(|v| v.kind.as_ref());
        assert!(matches!(
            val,
            Some(prost_types::value::Kind::StringValue(s)) if s == "from run.json"
        ));
    }
}

#[cfg(test)]
mod json_parse_tests {
    use super::*;

    fn pref(provider: &str, name: &str) -> vam_spec::v1::ModelPreference {
        vam_spec::v1::ModelPreference {
            provider: provider.into(),
            model_name: name.into(),
            temperature: 0.1,
            top_p: 1.0,
            max_completion_tokens: 4096,
        }
    }

    fn str_input(key: &str, v: &str) -> prost_types::Struct {
        prost_types::Struct {
            fields: [(
                key.to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(v.into())),
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    fn field(s: &prost_types::Struct, key: &str) -> Option<String> {
        string_from_struct_field(s, key)
    }

    #[test]
    fn gateway_ids_take_the_makers_context_window() {
        assert_eq!(
            context_window_for("anthropic/claude-haiku-5.5"),
            Some(200_000)
        );
        assert_eq!(context_window_for("claude-haiku-5-5"), Some(200_000));
        assert_eq!(context_window_for("qwen/qwen3-max"), None);
    }

    #[test]
    fn merge_agent_injects_provider_with_default_model() {
        let agent = AgentSpec {
            preferred_models: vec![pref("vercel", "anthropic/claude-haiku-5.5")],
            ..Default::default()
        };
        let merged = merge_agent_into_inputs(None, &agent).expect("expected struct");
        assert_eq!(field(&merged, "model_provider").as_deref(), Some("vercel"));
    }

    #[test]
    fn merge_agent_copies_provider_when_model_matches_preference() {
        let agent = AgentSpec {
            preferred_models: vec![
                pref("openrouter", "openai/gpt-4o-mini"),
                pref("vercel", "anthropic/claude-haiku-5.5"),
            ],
            ..Default::default()
        };
        let inputs = Some(str_input("model", "anthropic/claude-haiku-5.5"));
        let merged = merge_agent_into_inputs(inputs, &agent).expect("expected struct");
        assert_eq!(field(&merged, "model_provider").as_deref(), Some("vercel"));
    }

    #[test]
    fn merge_agent_keeps_existing_model_provider() {
        let agent = AgentSpec {
            preferred_models: vec![pref("vercel", "anthropic/claude-haiku-5.5")],
            ..Default::default()
        };
        let mut inputs = str_input("model", "anthropic/claude-haiku-5.5");
        inputs
            .fields
            .extend(str_input("model_provider", "openrouter").fields);
        let merged = merge_agent_into_inputs(Some(inputs), &agent).expect("expected struct");
        assert_eq!(
            field(&merged, "model_provider").as_deref(),
            Some("openrouter")
        );
    }

    #[test]
    fn merge_agent_omits_provider_when_no_preference_matches() {
        let agent = AgentSpec {
            preferred_models: vec![pref("vercel", "anthropic/claude-haiku-5.5")],
            ..Default::default()
        };
        let inputs = Some(str_input("model", "openai/gpt-4o-mini"));
        let merged = merge_agent_into_inputs(inputs, &agent).expect("expected struct");
        assert!(field(&merged, "model_provider").is_none());

        let agent = AgentSpec {
            preferred_models: vec![pref("", "openai/gpt-4o-mini")],
            ..Default::default()
        };
        let merged = merge_agent_into_inputs(None, &agent).expect("expected struct");
        assert!(field(&merged, "model_provider").is_none());
    }

    #[test]
    fn merge_agent_injects_preferred_model_when_missing() {
        let agent = AgentSpec {
            preferred_models: vec![vam_spec::v1::ModelPreference {
                provider: "openrouter".into(),
                model_name: "openai/gpt-4o-mini".into(),
                temperature: 0.1,
                top_p: 1.0,
                max_completion_tokens: 4096,
            }],
            ..Default::default()
        };
        let merged = merge_agent_into_inputs(None, &agent).expect("expected struct");
        let val = merged.fields.get("model").and_then(|v| v.kind.as_ref());
        assert!(matches!(
            val,
            Some(prost_types::value::Kind::StringValue(s)) if s == "openai/gpt-4o-mini"
        ));
    }

    #[test]
    fn merge_agent_preserves_run_json_model() {
        let agent = AgentSpec {
            preferred_models: vec![vam_spec::v1::ModelPreference {
                provider: "openrouter".into(),
                model_name: "openai/gpt-4o-mini".into(),
                temperature: 0.1,
                top_p: 1.0,
                max_completion_tokens: 4096,
            }],
            ..Default::default()
        };
        let inputs = Some(prost_types::Struct {
            fields: [(
                "model".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(
                        "anthropic/claude-3.5-sonnet".into(),
                    )),
                },
            )]
            .into_iter()
            .collect(),
        });
        let merged = merge_agent_into_inputs(inputs, &agent).expect("expected struct");
        let val = merged.fields.get("model").and_then(|v| v.kind.as_ref());
        assert!(matches!(
            val,
            Some(prost_types::value::Kind::StringValue(s)) if s == "anthropic/claude-3.5-sonnet"
        ));
    }

    #[test]
    fn bun_assistant_agent_json_has_system_instructions() {
        let raw = include_str!("../../../examples/bun-assistant/agent.json");
        let agent: vam_spec::v1::AgentSpec =
            vam_spec::encoding::json_to_proto("vam.v1.AgentSpec", raw).unwrap();
        let instructions = extract_system_instructions(&agent);
        assert!(
            instructions.is_some(),
            "agent.json must parse systemInstructions into instruction_source"
        );
        let s = instructions.unwrap();
        assert!(s.contains("calculator"), "instructions: {}", s);
        assert!(s.contains("write_file"), "instructions: {}", s);
    }

    #[test]
    fn bun_assistant_run_json_inputs_parse() {
        let raw = include_str!("../../../examples/bun-assistant/run.json");
        let req: vam_spec::v1::CreateRunRequest =
            vam_spec::encoding::json_to_proto("vam.v1.CreateRunRequest", raw).unwrap();
        let spec = req.spec.unwrap();
        let inputs = spec.inputs.expect("inputs");
        let prompt = inputs.fields.get("prompt").and_then(|v| v.kind.as_ref());
        assert!(
            matches!(prompt, Some(prost_types::value::Kind::StringValue(s)) if !s.is_empty()),
            "prompt must parse from flat JSON: {:?}",
            inputs.fields.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn merge_bun_assistant_agent_into_run_inputs() {
        let raw = include_str!("../../../examples/bun-assistant/agent.json");
        let agent: vam_spec::v1::AgentSpec =
            vam_spec::encoding::json_to_proto("vam.v1.AgentSpec", raw).unwrap();
        let merged = merge_agent_into_inputs(None, &agent).expect("expected struct");
        let instructions = merged
            .fields
            .get("system_instructions")
            .and_then(|v| v.kind.as_ref());
        assert!(matches!(
            instructions,
            Some(prost_types::value::Kind::StringValue(s))
                if s.contains("calculator") && s.contains("write_file")
        ));
    }
}

#[cfg(test)]
mod playbook_admission_tests {
    use super::*;
    use prost_types::value::Kind;
    use vam_spec::v1::{PlaybookStep, ResourceId, StepBinding, StepKind, StepMode, StepOutcome};

    const PLAYBOOK_URN: &str = "urn:vam:playbook:local.vam.dev/acme/cleanup";

    fn schema() -> Struct {
        json_struct(
            r#"{
                "type": "object",
                "additionalProperties": false,
                "required": ["project_key", "cutoff_days"],
                "properties": {
                    "project_key": {"type": "string"},
                    "cutoff_days": {"type": "integer"}
                }
            }"#,
        )
    }

    fn json_struct(json: &str) -> Struct {
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        json_to_struct(&value)
    }

    fn json_to_struct(value: &serde_json::Value) -> Struct {
        let serde_json::Value::Object(map) = value else {
            panic!("object");
        };
        let mut fields = std::collections::BTreeMap::new();
        for (key, field) in map {
            fields.insert(key.clone(), json_to_value(field));
        }
        Struct { fields }
    }

    fn json_to_value(value: &serde_json::Value) -> Value {
        let kind = match value {
            serde_json::Value::Null => Kind::NullValue(0),
            serde_json::Value::Bool(flag) => Kind::BoolValue(*flag),
            serde_json::Value::Number(number) => Kind::NumberValue(number.as_f64().unwrap()),
            serde_json::Value::String(text) => Kind::StringValue(text.clone()),
            serde_json::Value::Array(items) => Kind::ListValue(prost_types::ListValue {
                values: items.iter().map(json_to_value).collect(),
            }),
            serde_json::Value::Object(_) => Kind::StructValue(json_to_struct(value)),
        };
        Value { kind: Some(kind) }
    }

    fn string_value(text: &str) -> Value {
        Value {
            kind: Some(Kind::StringValue(text.to_string())),
        }
    }

    fn playbook() -> PlaybookSpec {
        PlaybookSpec {
            activation: Some(vam_spec::v1::PlaybookActivation::asking("Use when the request matches this test playbook.")),
            resource_id: Some(ResourceId {
                urn: PLAYBOOK_URN.to_string(),
            }),
            display_name: "Sprint cleanup".to_string(),
            description: "Close stale issues.".to_string(),
            input_schema: Some(schema()),
            steps: vec![
                PlaybookStep {
                    id: "query".to_string(),
                    kind: StepKind::Task as i32,
                    mode: StepMode::Agentic as i32,
                    instruction: "Search {{inputs.project_key}} older than {{inputs.cutoff_days}}."
                        .to_string(),
                    ..Default::default()
                },
                PlaybookStep {
                    id: "approve".to_string(),
                    kind: StepKind::Gate as i32,
                    question: "Approve {{steps.query.output.count}} ({{steps.query.output.issues[*].key}})?"
                        .to_string(),
                    after: vec!["query".to_string()],
                    options: vec![
                        StepOutcome {
                            label: "approved".to_string(),
                            ..Default::default()
                        },
                        StepOutcome {
                            label: "rejected".to_string(),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn inputs_with_model() -> Struct {
        let mut inputs = vam_spec::parse_playbook_inputs(
            Some(&schema()),
            &[
                ("project_key".to_string(), "PROJ".to_string()),
                ("cutoff_days".to_string(), "14".to_string()),
            ],
        )
        .unwrap();
        inputs
            .fields
            .insert("model".to_string(), string_value("openai/gpt-4o-mini"));
        inputs
    }

    fn admitted() -> (SqliteWorkflowStore, RunSpec, PreparedPlaybookAdmission) {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .put_playbook_with_owner(&playbook(), crate::users::LOCAL_USER_ID)
            .expect("publish playbook");
        let mut spec = RunSpec {
            playbook_ref: Some(ResourceRef {
                resource_id: Some(ResourceId {
                    urn: PLAYBOOK_URN.to_string(),
                }),
                ..Default::default()
            }),
            inputs: Some(inputs_with_model()),
            ..Default::default()
        };
        crate::catalog_resolve::record_resolved_refs(
            &store,
            &mut spec,
            &AgentSpec::default(),
            crate::users::LOCAL_USER_ID,
            false,
        )
        .expect("pin playbook");
        let prepared =
            prepare_playbook_admission(&store, &spec, crate::users::LOCAL_USER_ID, false)
                .expect("admit")
                .expect("playbook run");
        (store, spec, prepared)
    }

    #[test]
    fn procedure_renders_inputs_and_step_paths() {
        let rendered = render_playbook_procedure(
            &playbook(),
            Some(&inputs_with_model()),
            &[(
                "query",
                r#"{"count":3,"issues":[{"key":"PROJ-1"},{"key":"PROJ-2"}]}"#,
            )],
        );
        assert!(rendered.contains("# Sprint cleanup"));
        assert!(rendered.contains("Search PROJ older than 14."));
        assert!(rendered.contains("Approve 3 (PROJ-1, PROJ-2)?"));
        assert!(rendered.contains("kind: task"));
        assert!(rendered.contains("kind: gate"));
        assert!(rendered.contains("after: query"));
        assert!(rendered.contains("outcomes: approved, rejected"));
        assert!(rendered.contains("Each step names the tool that completes it."));
        assert!(rendered.contains("mode: agentic"));
        assert!(rendered.contains(
            "Do this step yourself with the tools you have, then call step_complete with step_id \"query\" and an outcome describing what you did. Never invent results. If you cannot do it with your tools, say so in the outcome."
        ));
        assert!(rendered.contains(
            "Wait for the human decision on the gate approval. Do not call run_step or step_complete for it."
        ));
        assert!(!rendered.contains("{{inputs.project_key}}"));

        let pending = render_playbook_procedure(&playbook(), Some(&inputs_with_model()), &[]);
        assert!(pending.contains("{{steps.query.output.count}}"));
        assert!(pending.contains("Search PROJ older than 14."));
    }

    #[test]
    fn procedure_names_mode_and_completion_action() {
        let playbook = PlaybookSpec {
            activation: Some(vam_spec::v1::PlaybookActivation::asking(
                "Use when the request matches this test playbook.",
            )),
            display_name: "Mixed modes".to_string(),
            steps: vec![
                PlaybookStep {
                    id: "lookup".to_string(),
                    kind: StepKind::Task as i32,
                    mode: StepMode::Bound as i32,
                    instruction: "Look up the record.".to_string(),
                    binding: Some(StepBinding {
                        tool_name: "echo".to_string(),
                        arguments: None,
                    }),
                    ..Default::default()
                },
                PlaybookStep {
                    id: "fill".to_string(),
                    kind: StepKind::Task as i32,
                    mode: StepMode::Assisted as i32,
                    instruction: "Fill the search.".to_string(),
                    binding: Some(StepBinding {
                        tool_name: "search".to_string(),
                        arguments: None,
                    }),
                    ..Default::default()
                },
                PlaybookStep {
                    id: "extract".to_string(),
                    kind: StepKind::Task as i32,
                    mode: StepMode::Agentic as i32,
                    instruction: "Record the reference.".to_string(),
                    ..Default::default()
                },
                PlaybookStep {
                    id: "approve".to_string(),
                    kind: StepKind::Gate as i32,
                    question: "Approve it?".to_string(),
                    after: vec!["extract".to_string()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let rendered = render_playbook_procedure(&playbook, None, &[]);
        assert!(rendered.contains(
            "Complete available steps in any order. The host tracks each step. Each step names the tool that completes it."
        ));

        let lookup = section_after(&rendered, "## lookup");
        assert!(lookup.contains("kind: task"));
        assert!(lookup.contains("mode: bound"));
        assert!(lookup.contains(
            "Complete it by calling run_step with step_id \"lookup\". The host runs the bound tool."
        ));

        let fill = section_after(&rendered, "## fill");
        assert!(fill.contains("mode: assisted"));
        assert!(fill.contains(
            "Call \"search\" yourself, filling its arguments from context, then call step_complete with step_id \"fill\" and an outcome describing what you did. Do not call run_step for it."
        ));

        let extract = section_after(&rendered, "## extract");
        assert!(extract.contains("mode: agentic"));
        assert!(extract.contains(
            "Do this step yourself with the tools you have, then call step_complete with step_id \"extract\" and an outcome describing what you did. Never invent results. If you cannot do it with your tools, say so in the outcome."
        ));

        let approve = section_after(&rendered, "## approve");
        assert!(approve.contains("kind: gate"));
        assert!(!approve.contains("mode:"));
        assert!(approve.contains(
            "Wait for the human decision on the gate approval. Do not call run_step or step_complete for it."
        ));
    }

    fn section_after<'a>(rendered: &'a str, heading: &str) -> &'a str {
        let rest = rendered
            .split_once(heading)
            .map(|(_, after)| after)
            .unwrap_or("");
        rest.split("\n## ").next().unwrap_or(rest)
    }

    #[test]
    fn playbook_admission_loads_pin_validates_inputs_and_inits_steps() {
        let (store, spec, prepared) = admitted();
        assert_eq!(prepared.pin.urn, PLAYBOOK_URN);
        assert_eq!(prepared.pin.version, "1.0.0");
        assert!(!prepared.pin.digest.is_empty());
        assert!(prepared.procedure.contains("Search PROJ older than 14."));
        assert!(prepared.procedure.contains("{{steps.query.output.count}}"));

        let run_id = "urn:vam:run:local.vam.dev/acme/cleanup-run";
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("run");
        commit_playbook_admission(&store, run_id, &prepared).expect("init steps");
        let steps = store.list_playbook_steps(run_id).expect("steps");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].step_id, "query");
        assert_eq!(
            steps[0].state,
            crate::workflow::PlaybookStepState::Available
        );
        assert_eq!(steps[1].step_id, "approve");
        assert_eq!(steps[1].state, crate::workflow::PlaybookStepState::Pending);

        let delivered = insert_playbook_procedure(spec.inputs.clone(), &prepared.procedure);
        let procedure = delivered
            .fields
            .get(PLAYBOOK_PROCEDURE_FIELD)
            .and_then(|value| value.kind.as_ref());
        assert!(matches!(
            procedure,
            Some(Kind::StringValue(text)) if text.contains("Search PROJ older than 14.")
        ));
    }

    #[test]
    fn playbook_admission_ignores_host_owned_inputs_under_a_strict_schema() {
        // The schema forbids extra properties, so host-owned run inputs must be
        // stripped before the check or every app run with a playbook is rejected.
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .put_playbook_with_owner(&playbook(), crate::users::LOCAL_USER_ID)
            .expect("publish playbook");
        let mut inputs = inputs_with_model();
        inputs
            .fields
            .insert("max_iterations".to_string(), string_value("15"));
        inputs
            .fields
            .insert("step_timeout_secs".to_string(), string_value("300"));
        let mut spec = RunSpec {
            playbook_ref: Some(ResourceRef {
                resource_id: Some(ResourceId {
                    urn: PLAYBOOK_URN.to_string(),
                }),
                ..Default::default()
            }),
            inputs: Some(inputs),
            ..Default::default()
        };
        crate::catalog_resolve::record_resolved_refs(
            &store,
            &mut spec,
            &AgentSpec::default(),
            crate::users::LOCAL_USER_ID,
            false,
        )
        .expect("pin");
        prepare_playbook_admission(&store, &spec, crate::users::LOCAL_USER_ID, false)
            .expect("host-owned inputs are not playbook inputs");
    }

    #[test]
    fn playbook_admission_accepts_chat_prompt_under_a_strict_schema() {
        // Every app chat run sends `prompt` (and `model`). Under a strict schema
        // that does not declare them, admission must still pass, while any other
        // undeclared key stays rejected.
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .put_playbook_with_owner(&playbook(), crate::users::LOCAL_USER_ID)
            .expect("publish playbook");
        let mut inputs = inputs_with_model();
        inputs
            .fields
            .insert("prompt".to_string(), string_value("Hi"));
        let mut spec = RunSpec {
            playbook_ref: Some(ResourceRef {
                resource_id: Some(ResourceId {
                    urn: PLAYBOOK_URN.to_string(),
                }),
                ..Default::default()
            }),
            inputs: Some(inputs),
            ..Default::default()
        };
        crate::catalog_resolve::record_resolved_refs(
            &store,
            &mut spec,
            &AgentSpec::default(),
            crate::users::LOCAL_USER_ID,
            false,
        )
        .expect("pin");
        prepare_playbook_admission(&store, &spec, crate::users::LOCAL_USER_ID, false)
            .expect("chat prompt is a host input, not a playbook input");

        spec.inputs
            .as_mut()
            .expect("inputs")
            .fields
            .insert("bogus".to_string(), string_value("x"));
        let err = prepare_playbook_admission(&store, &spec, crate::users::LOCAL_USER_ID, false)
            .expect_err("undeclared author input");
        let message = err.to_string();
        assert!(message.contains("inputs.bogus"), "{message}");
        assert!(message.contains("not declared"), "{message}");
    }

    #[test]
    fn playbook_admission_rejects_inputs_that_fail_the_schema() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .put_playbook_with_owner(&playbook(), crate::users::LOCAL_USER_ID)
            .expect("publish playbook");
        let mut spec = RunSpec {
            playbook_ref: Some(ResourceRef {
                resource_id: Some(ResourceId {
                    urn: PLAYBOOK_URN.to_string(),
                }),
                ..Default::default()
            }),
            inputs: Some(Struct {
                fields: [("project_key".to_string(), string_value("PROJ"))]
                    .into_iter()
                    .collect(),
            }),
            ..Default::default()
        };
        crate::catalog_resolve::record_resolved_refs(
            &store,
            &mut spec,
            &AgentSpec::default(),
            crate::users::LOCAL_USER_ID,
            false,
        )
        .expect("pin");
        let err = prepare_playbook_admission(&store, &spec, crate::users::LOCAL_USER_ID, false)
            .expect_err("missing cutoff");
        let message = err.to_string();
        assert!(message.contains("inputs.cutoff_days"), "{message}");
        assert!(message.contains("required"), "{message}");
    }

    #[test]
    fn runs_without_a_playbook_skip_admission() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let spec = RunSpec::default();
        assert!(
            prepare_playbook_admission(&store, &spec, crate::users::LOCAL_USER_ID, false)
                .expect("no playbook")
                .is_none()
        );
    }

    #[test]
    fn admission_rejects_foreign_draft_playbook_pin() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        store
            .publish_playbook_draft_with_owner(&playbook(), "alice")
            .expect("alice draft");
        let draft = vam_spec::v1::Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: "draft".to_string(),
            build: String::new(),
        };
        let pin = store
            .catalog_pin_at_version("playbooks", PLAYBOOK_URN, &draft, "alice", false)
            .expect("pin");
        let spec = RunSpec {
            playbook_ref: Some(ResourceRef {
                resource_id: Some(ResourceId {
                    urn: PLAYBOOK_URN.to_string(),
                }),
                version: Some(draft),
                digest: Some(vam_spec::v1::Digest {
                    algorithm: 1,
                    hex_hash: pin.digest,
                }),
                ..Default::default()
            }),
            inputs: Some(inputs_with_model()),
            ..Default::default()
        };
        let err = prepare_playbook_admission(&store, &spec, "bob", false).expect_err("foreign");
        assert_eq!(
            err.to_string(),
            format!("not found: no draft of playbook '{PLAYBOOK_URN}' for this user")
        );
        prepare_playbook_admission(&store, &spec, "alice", false).expect("owner");
        prepare_playbook_admission(&store, &spec, "bob", true).expect("operator");
    }
}

#[cfg(test)]
#[path = "run_context_harness_tests.rs"]
mod resolve_harness_tests;

#[cfg(test)]
mod resolve_run_budgets_tests {
    use super::*;
    use prost_types::value::Kind;
    use vam_spec::v1::ReasoningPolicy;

    fn agent(policy: Option<ReasoningPolicy>) -> AgentSpec {
        AgentSpec {
            reasoning_policy: policy,
            ..Default::default()
        }
    }

    fn policy(iterations: i32, timeout_secs: Option<i64>) -> ReasoningPolicy {
        ReasoningPolicy {
            max_iterations: iterations,
            step_timeout: timeout_secs.map(|seconds| prost_types::Duration { seconds, nanos: 0 }),
            termination: 0,
        }
    }

    fn string_input(text: &str) -> Value {
        Value {
            kind: Some(Kind::StringValue(text.to_string())),
        }
    }

    fn number_input(number: f64) -> Value {
        Value {
            kind: Some(Kind::NumberValue(number)),
        }
    }

    fn budgets(spec: &RunSpec) -> (String, String, i64) {
        let inputs = spec.inputs.as_ref().expect("inputs");
        let iterations = match inputs
            .fields
            .get("max_iterations")
            .and_then(|v| v.kind.as_ref())
        {
            Some(Kind::StringValue(text)) => text.clone(),
            other => panic!("max_iterations string, got {other:?}"),
        };
        let step = match inputs
            .fields
            .get("step_timeout_secs")
            .and_then(|v| v.kind.as_ref())
        {
            Some(Kind::StringValue(text)) => text.clone(),
            other => panic!("step_timeout_secs string, got {other:?}"),
        };
        let duration = spec.max_duration.as_ref().expect("max_duration").seconds;
        (iterations, step, duration)
    }

    fn termination_input(spec: &RunSpec) -> Option<String> {
        match spec
            .inputs
            .as_ref()
            .and_then(|inputs| inputs.fields.get("termination"))
            .and_then(|v| v.kind.as_ref())
        {
            Some(Kind::StringValue(text)) => Some(text.clone()),
            _ => None,
        }
    }

    #[test]
    fn termination_is_stored_in_the_run_inputs_and_survives_a_spec_round_trip() {
        use vam_spec::v1::TerminationPolicy;
        let mut policy = policy(8, None);
        policy.termination = TerminationPolicy::ExplicitClaim as i32;
        let mut spec = RunSpec::default();
        resolve_run_budgets(&mut spec, &agent(Some(policy.clone())));
        assert_eq!(termination_input(&spec).as_deref(), Some("explicit_claim"));
        // Resume reads the stored spec; it is not resolved again.
        let json = vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &spec).expect("to json");
        let stored: RunSpec =
            vam_spec::encoding::json_to_proto("vam.v1.RunSpec", &json).expect("from json");
        assert_eq!(
            termination_input(&stored).as_deref(),
            Some("explicit_claim")
        );

        policy.termination = TerminationPolicy::OutputSatisfied as i32;
        let mut satisfied = RunSpec::default();
        resolve_run_budgets(&mut satisfied, &agent(Some(policy)));
        assert_eq!(
            termination_input(&satisfied).as_deref(),
            Some("output_satisfied")
        );
    }

    #[test]
    fn unset_termination_removes_a_caller_supplied_value() {
        let mut spec = RunSpec {
            inputs: Some(prost_types::Struct {
                fields: [("termination".to_string(), string_input("explicit_claim"))]
                    .into_iter()
                    .collect(),
            }),
            ..Default::default()
        };
        resolve_run_budgets(&mut spec, &agent(Some(policy(8, None))));
        assert_eq!(termination_input(&spec), None);
    }

    #[test]
    fn unset_policy_uses_the_defaults() {
        let mut spec = RunSpec {
            inputs: Some(prost_types::Struct {
                fields: [("prompt".to_string(), string_input("hello"))]
                    .into_iter()
                    .collect(),
            }),
            ..Default::default()
        };
        resolve_run_budgets(&mut spec, &agent(None));
        assert_eq!(budgets(&spec), ("50".to_string(), "300".to_string(), 1_800));
        assert_eq!(
            spec.inputs
                .as_ref()
                .and_then(|inputs| inputs.fields.get("prompt"))
                .and_then(|value| value.kind.as_ref()),
            Some(&Kind::StringValue("hello".to_string()))
        );
    }

    #[test]
    fn agent_policy_is_copied_and_iterations_are_capped() {
        let mut within = RunSpec::default();
        resolve_run_budgets(&mut within, &agent(Some(policy(8, Some(60)))));
        assert_eq!(budgets(&within), ("8".to_string(), "60".to_string(), 1_800));

        let mut capped = RunSpec::default();
        resolve_run_budgets(&mut capped, &agent(Some(policy(500, None))));
        assert_eq!(
            budgets(&capped),
            ("200".to_string(), "300".to_string(), 1_800)
        );
    }

    #[test]
    fn caller_iterations_may_lower_the_resolved_budget() {
        let mut lowered = RunSpec {
            inputs: Some(prost_types::Struct {
                fields: [("max_iterations".to_string(), string_input("15"))]
                    .into_iter()
                    .collect(),
            }),
            ..Default::default()
        };
        resolve_run_budgets(&mut lowered, &agent(None));
        assert_eq!(budgets(&lowered).0, "15");

        let mut raised = RunSpec {
            inputs: Some(prost_types::Struct {
                fields: [("max_iterations".to_string(), number_input(99.0))]
                    .into_iter()
                    .collect(),
            }),
            ..Default::default()
        };
        resolve_run_budgets(&mut raised, &agent(None));
        assert_eq!(budgets(&raised).0, "50");

        let mut above_agent = RunSpec {
            inputs: Some(prost_types::Struct {
                fields: [("max_iterations".to_string(), string_input("15"))]
                    .into_iter()
                    .collect(),
            }),
            ..Default::default()
        };
        resolve_run_budgets(&mut above_agent, &agent(Some(policy(8, Some(60)))));
        assert_eq!(budgets(&above_agent).0, "8");
        assert_eq!(budgets(&above_agent).1, "60");
    }

    #[test]
    fn caller_step_timeout_is_replaced_and_positive_duration_is_kept() {
        let mut spec = RunSpec {
            inputs: Some(prost_types::Struct {
                fields: [("step_timeout_secs".to_string(), string_input("10"))]
                    .into_iter()
                    .collect(),
            }),
            max_duration: Some(prost_types::Duration {
                seconds: 600,
                nanos: 0,
            }),
            ..Default::default()
        };
        resolve_run_budgets(&mut spec, &agent(Some(policy(8, Some(60)))));
        assert_eq!(budgets(&spec), ("8".to_string(), "60".to_string(), 600));

        let mut unset = RunSpec {
            max_duration: Some(prost_types::Duration {
                seconds: 0,
                nanos: 0,
            }),
            ..Default::default()
        };
        resolve_run_budgets(&mut unset, &agent(None));
        assert_eq!(budgets(&unset).2, 1_800);
    }

    // ---- Playbooks in flight (AR-0.11) -------------------------------------

    fn outputs(pairs: &[(&str, serde_json::Value)]) -> BTreeMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn args_struct(json: serde_json::Value) -> Struct {
        crate::playbook_activation::json_to_struct(&json).expect("struct")
    }

    #[test]
    fn bound_arguments_resolve_with_types_text_and_sources() {
        let steps = outputs(&[
            (
                "pick",
                serde_json::json!({"id": "S2", "qty": 3, "tags": ["a", "b"]}),
            ),
            ("find", serde_json::json!({"names": ["x", "y"]})),
        ]);
        let inputs = serde_json::json!({"order": "O-9"});
        let resolved = resolve_binding_arguments(
            Some(&args_struct(serde_json::json!({
                "id": "{{steps.pick.output.id}}",
                "qty": "{{steps.pick.output.qty}}",
                "tags": "{{steps.pick.output.tags}}",
                "label": "order {{inputs.order}} for {{steps.pick.output.id}}",
                "nested": {"names": "{{steps.find.output.names[*]}}"},
                "plain": "no templates"
            }))),
            &inputs,
            &steps,
        )
        .expect("resolves");
        let json = struct_to_json(&resolved.arguments);
        assert_eq!(json["id"], "S2");
        assert_eq!(json["qty"], 3.0);
        assert_eq!(json["tags"], serde_json::json!(["a", "b"]));
        assert_eq!(json["label"], "order O-9 for S2");
        assert_eq!(json["nested"]["names"], serde_json::json!(["x", "y"]));
        assert_eq!(json["plain"], "no templates");
        assert_eq!(
            resolved.sources.into_iter().collect::<Vec<_>>(),
            vec![
                TemplateSource::Inputs,
                TemplateSource::Step("find".to_string()),
                TemplateSource::Step("pick".to_string())
            ]
        );
    }

    #[test]
    fn an_unresolved_template_is_an_error_never_a_literal_token() {
        let steps = outputs(&[("pick", serde_json::json!({"id": "S2"}))]);
        for bad in [
            "{{steps.later.output.id}}",
            "{{steps.pick.output.missing}}",
            "{{inputs.nothing}}",
            "x {{steps.pick.output.nope}} y",
        ] {
            let err = resolve_binding_arguments(
                Some(&args_struct(serde_json::json!({"a": bad}))),
                &serde_json::json!({}),
                &steps,
            )
            .expect_err(bad);
            assert!(err.contains("cannot resolve"), "{err}");
        }
    }

    #[test]
    fn gate_defaults_are_typed_and_an_unresolved_default_is_dropped() {
        let steps = outputs(&[(
            "extract",
            serde_json::json!({"supplier": "Acme", "amount": 12.5}),
        )]);
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "default": "{{steps.extract.output.supplier}}"},
                "amount": {"type": "number", "default": "{{steps.extract.output.amount}}"},
                "later": {"type": "string", "default": "{{steps.nope.output.x}}", "title": "Later {{steps.extract.output.supplier}}"}
            }
        });
        render_schema_templates(&mut schema, &serde_json::json!({}), &steps);
        assert_eq!(schema["properties"]["name"]["default"], "Acme");
        assert_eq!(schema["properties"]["amount"]["default"], 12.5);
        assert!(schema["properties"]["later"].get("default").is_none());
        assert_eq!(schema["properties"]["later"]["title"], "Later Acme");
    }

    #[test]
    fn start_inputs_win_over_the_flat_run_inputs() {
        let inputs = args_struct(serde_json::json!({
            "prompt": "hello",
            "playbook_inputs": {"order_id": "O-1"}
        }));
        assert_eq!(
            template_inputs_json(Some(&inputs)),
            serde_json::json!({"order_id": "O-1"})
        );
        let flat = args_struct(serde_json::json!({"order_id": "O-2"}));
        assert_eq!(
            template_inputs_json(Some(&flat)),
            serde_json::json!({"order_id": "O-2"})
        );
    }

    fn status_spec() -> PlaybookSpec {
        vam_spec::encoding::json_to_proto(
            "vam.v1.PlaybookSpec",
            include_str!("../../../examples/oracle-purchase-requisition/playbook.json"),
        )
        .expect("oracle")
    }

    fn record(
        id: &str,
        state: crate::workflow::PlaybookStepState,
        outcome: &str,
        output: Option<&str>,
    ) -> crate::workflow::PlaybookStepRecord {
        crate::workflow::PlaybookStepRecord {
            step_id: id.to_string(),
            state,
            outcome: outcome.to_string(),
            output_json: output.map(str::to_string),
            agent_summary: None,
            evidence_refs: vec![],
            started_at: None,
            finished_at: None,
            transition_revision: 1,
        }
    }

    #[test]
    fn the_status_block_lists_done_captured_available_and_the_awaited_gate() {
        use crate::workflow::PlaybookStepState::*;
        let nonce = vam_security::vendor_text::FenceNonce::from_bytes([9u8; 16]);
        let records = vec![
            record("mode", Completed, "guided", Some(r#"{"option":"guided"}"#)),
            record("get-invoice", Skipped, "", None),
            record(
                "supplier-name",
                Completed,
                "submitted",
                Some(r#"{"name":"Acme"}"#),
            ),
            record(
                "find-suppliers",
                Completed,
                "done",
                Some(r#"{"suppliers":["S1","S2","S3","S4","S5"]}"#),
            ),
            record("pick-supplier", Open, "", None),
        ];
        let text = render_playbook_status(&status_spec(), &records, None, &nonce);
        assert!(
            text.starts_with("[Playbook status: Oracle purchase requisition]\n"),
            "{text}"
        );
        assert!(
            text.contains("Done: mode=guided, supplier-name=submitted, find-suppliers=done"),
            "{text}"
        );
        assert!(text.contains("- supplier-name.name = \"Acme\""), "{text}");
        assert!(text.contains("Available now: none"), "{text}");
        assert!(text.contains("Waiting for the user (do not ask in chat; the form is open): pick-supplier: \"Which supplier do you mean?\""), "{text}");
        assert!(text.contains("Skipped: get-invoice"), "{text}");
        // Bound output is fenced; arrays are summarised.
        let fenced_at = text.find("<<<").expect("fence");
        assert!(
            text[fenced_at..]
                .contains("find-suppliers.suppliers = [\"S1\", \"S2\", \"S3\", … +2 more]"),
            "{text}"
        );
        assert!(
            !text[..fenced_at].contains("find-suppliers.suppliers"),
            "{text}"
        );
    }

    #[test]
    fn the_status_block_is_bounded() {
        use crate::workflow::PlaybookStepState::*;
        let nonce = vam_security::vendor_text::FenceNonce::from_bytes([9u8; 16]);
        let big = "z".repeat(5000);
        let records = vec![
            record(
                "mode",
                Completed,
                "guided",
                Some(&serde_json::json!({"option": big}).to_string()),
            ),
            record(
                "supplier-name",
                Completed,
                "submitted",
                Some(r#"{"name":"Acme"}"#),
            ),
        ];
        let text = render_playbook_status(&status_spec(), &records, None, &nonce);
        assert!(
            text.chars().count() < STATUS_VALUES_MAX_CHARS + 1500,
            "{}",
            text.chars().count()
        );
        assert!(!text.contains(&"z".repeat(STATUS_VALUE_MAX_CHARS + 1)));
        let many: Vec<_> = (0..60)
            .map(|i| (format!("k{i:02}"), serde_json::json!("v".repeat(150))))
            .collect();
        let output = serde_json::Value::Object(many.into_iter().collect()).to_string();
        let records = vec![record("mode", Completed, "guided", Some(&output))];
        let text = render_playbook_status(&status_spec(), &records, None, &nonce);
        assert!(text.contains("older captured values left out"), "{text}");
        assert!(text.chars().count() < STATUS_VALUES_MAX_CHARS + 1500);
    }

    #[test]
    fn the_procedure_carries_the_gate_rule() {
        let text = render_playbook_procedure(&status_spec(), None, &[]);
        assert!(text.contains(vam_spec::playbook::PLAYBOOK_GATE_RULE));
    }
}
