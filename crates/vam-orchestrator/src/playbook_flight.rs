//! Playbooks in flight (decision 21): what the host does while a bound playbook runs.
//!
//! - Bound-step arguments are resolved from stored state only (gate responses,
//!   start inputs, earlier outputs), never from the guest's arguments.
//! - Provenance rule: when a templated bound step would run a tool that can
//!   change something, and any resolved value did not come from the user (a gate
//!   response or the validated start inputs), the user approves the resolved
//!   arguments first.
//! - The status block the model reads after each step event.
//!
//! Nothing here grants a permission. Cedar decides every call.

use std::collections::BTreeSet;

use prost_types::Struct;
use vam_spec::v1::{SideEffectClass, StepKind};

use crate::run_context::{resolve_binding_arguments, template_inputs_json, TemplateSource};
use crate::workflow::{PlaybookStepRecord, SqliteWorkflowStore};

/// The host tool name of a bound-step run.
pub const RUN_STEP_TOOL: &str = "run_step";
/// Most characters of a bound tool's result kept as a step output when it is not a JSON object.
pub const CAPTURED_TEXT_MAX_CHARS: usize = 8 * 1024;

/// What `run_step` would execute for one bound step.
#[derive(Debug, Clone)]
pub struct BoundStepPlan {
    pub step_id: String,
    pub tool_name: String,
    /// The literal arguments, or the templates resolved from stored state.
    pub arguments: Struct,
    pub templated: bool,
    pub sources: BTreeSet<TemplateSource>,
    /// A person must approve these exact arguments before the step runs.
    pub ask: bool,
    /// Why, in words for the approval card.
    pub ask_reason: String,
    /// Digest of `{step_id, arguments}`: the grant key and the approval's args digest.
    pub grant_digest: String,
    /// `{step_id, arguments}` as the approval stores and shows it.
    pub requested: serde_json::Value,
}

fn is_templated(arguments: Option<&Struct>) -> bool {
    arguments.is_some_and(|args| {
        crate::run_context::struct_json(args)
            .to_string()
            .contains("{{")
    })
}

/// Resolve a bound step from the stored run state and decide whether to ask.
pub fn plan_bound_step(
    store: &SqliteWorkflowStore,
    session_id: &str,
    run_id: &str,
    step_id: &str,
) -> Result<BoundStepPlan, String> {
    let spec = store.get_playbook_spec(run_id).map_err(|e| e.to_string())?;
    let step = spec
        .steps
        .iter()
        .find(|step| step.id == step_id)
        .ok_or_else(|| format!("run_step: step '{step_id}' was not found"))?;
    let binding = step
        .binding
        .as_ref()
        .ok_or_else(|| format!("run_step only executes bound steps; '{step_id}' has no binding"))?;
    let templated = is_templated(binding.arguments.as_ref());
    let (arguments, sources) = if templated {
        let records = store
            .list_playbook_steps(run_id)
            .map_err(|e| e.to_string())?;
        let inputs = store.run_template_inputs(run_id);
        let resolved = resolve_binding_arguments(
            binding.arguments.as_ref(),
            &template_inputs_json(inputs.as_ref()),
            &crate::run_context::step_outputs_map(&records),
        )
        .map_err(|e| format!("run_step: step '{step_id}': {e}"))?;
        (resolved.arguments, resolved.sources)
    } else {
        (
            binding.arguments.clone().unwrap_or_default(),
            BTreeSet::new(),
        )
    };

    let not_user: Vec<String> = sources
        .iter()
        .filter_map(|source| match source {
            TemplateSource::Inputs => None,
            TemplateSource::Step(id) => {
                let gate = spec.steps.iter().any(|s| {
                    s.id == *id
                        && StepKind::try_from(s.kind).unwrap_or(StepKind::Task) == StepKind::Gate
                });
                (!gate).then(|| id.clone())
            }
        })
        .collect();
    let read_only = store
        .get_tool(session_id, &binding.tool_name)
        .ok()
        .flatten()
        .is_some_and(|entry| entry.side_effect == SideEffectClass::ReadOnly as i32);
    let ask = templated && !not_user.is_empty() && !read_only;
    let ask_reason = if ask {
        format!(
            "Values for '{}' come from the output of {} rather than from you.",
            binding.tool_name,
            not_user
                .iter()
                .map(|id| format!("step '{id}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        String::new()
    };
    let requested = serde_json::json!({
        "step_id": step_id,
        "arguments": crate::run_context::struct_json(&arguments),
    });
    let grant_digest = crate::playbook_activation::json_to_struct(&requested)
        .and_then(|s| vam_spec::canonical_args_digest(Some(&s)).ok())
        .ok_or_else(|| "run_step: resolved arguments cannot be canonicalized".to_string())?;
    Ok(BoundStepPlan {
        step_id: step_id.to_string(),
        tool_name: binding.tool_name.clone(),
        arguments,
        templated,
        sources,
        ask,
        ask_reason,
        grant_digest,
        requested,
    })
}

/// The status block for a run's playbook, or `None` when no playbook is bound
/// (a guidance-only playbook tracks nothing).
pub fn status_block(store: &SqliteWorkflowStore, run_id: &str) -> Option<String> {
    store.get_playbook_run(run_id).ok().flatten()?;
    let spec = store.get_playbook_spec(run_id).ok()?;
    let records: Vec<PlaybookStepRecord> = store.list_playbook_steps(run_id).ok()?;
    let inputs = store.run_template_inputs(run_id);
    Some(crate::run_context::render_playbook_status(
        &spec,
        &records,
        inputs.as_ref(),
        &crate::host_tools::new_fence_nonce(),
    ))
}

/// The step output recorded from a bound tool's result: the JSON object when
/// the result is one, otherwise `{"result": <text>}` with the text capped.
pub fn captured_output_json(result_text: &str) -> String {
    let body = vam_security::vendor_text::fenced_body(result_text)
        .unwrap_or_else(|| result_text.to_string());
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body.trim()) {
        if value.is_object() {
            return value.to_string();
        }
    }
    let capped: String = body.chars().take(CAPTURED_TEXT_MAX_CHARS).collect();
    serde_json::json!({ "result": capped }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_output_is_the_object_or_wrapped_text() {
        assert_eq!(captured_output_json(r#"{"a":1}"#), r#"{"a":1}"#);
        assert_eq!(captured_output_json("ok"), r#"{"result":"ok"}"#);
        assert_eq!(captured_output_json("[1,2]"), r#"{"result":"[1,2]"}"#);
        let long = "x".repeat(CAPTURED_TEXT_MAX_CHARS + 50);
        let value: serde_json::Value = serde_json::from_str(&captured_output_json(&long)).unwrap();
        assert_eq!(
            value["result"].as_str().unwrap().len(),
            CAPTURED_TEXT_MAX_CHARS
        );
    }

    #[test]
    fn fenced_objects_are_unfenced_before_capture() {
        let nonce = vam_security::vendor_text::FenceNonce::from_bytes([3u8; 16]);
        let fenced = vam_security::vendor_text::fence(
            "mcp.x",
            vam_security::vendor_text::VendorTextKind::ToolResult,
            &nonce,
            r#"{"suppliers":[{"id":"S1"}]}"#,
        );
        assert_eq!(
            captured_output_json(&fenced.text),
            r#"{"suppliers":[{"id":"S1"}]}"#
        );
    }
}
