//! Playbook activation (AR-0.9): which playbooks a run offers the model, and the
//! `start_playbook` host tool that binds one to the running task.
//!
//! Starting a playbook never grants a permission. Cedar decides every call, as
//! before. The host only records the pin and step state, and decides whether a
//! person must confirm first.

use prost_types::value::Kind;
use prost_types::{Struct, Value};
use serde::{Deserialize, Serialize};
use vam_spec::v1::{
    ActivationMode, AgentSpec, PlaybookSpec, ResourceRef, RunSpec, SideEffectClass, StepKind,
    StepMode,
};

use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

/// Run-input field carrying the playbooks offered to the model (a JSON array
/// of [`AttachedPlaybook`], as a string).
pub const ATTACHED_PLAYBOOKS_FIELD: &str = "attached_playbooks";
/// Name of the host tool the model calls to start a playbook.
pub const START_PLAYBOOK_TOOL: &str = "start_playbook";
/// Outbox event written when a playbook is bound to a running task.
pub const EVENT_PLAYBOOK_ACTIVATED: &str = "vam.v1.playbook.activated";

/// One playbook offered to the model, with the text it matches a request on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachedPlaybook {
    pub urn: String,
    pub name: String,
    pub when: String,
    /// The agent's main job. Listed first.
    #[serde(default)]
    pub default: bool,
}

/// The playbooks an agent offers: the default first, then the allowed refs.
///
/// `allow_any_playbook` does not widen the list. Only refs the run pinned are
/// offered. A playbook with no `activation.when` can only be an unreleased
/// draft (publish requires it), and has nothing to match on, so it is left out.
pub fn build_attached(
    store: &SqliteWorkflowStore,
    spec: &RunSpec,
    agent: &AgentSpec,
    owner_user_id: &str,
    include_foreign_drafts: bool,
) -> Result<Vec<AttachedPlaybook>, OrchestratorError> {
    let mut out: Vec<AttachedPlaybook> = Vec::new();
    let default_urn = agent
        .default_playbook_ref
        .as_ref()
        .and_then(|r| r.resource_id.as_ref())
        .map(|id| id.urn.clone())
        .unwrap_or_default();
    let refs = agent
        .default_playbook_ref
        .iter()
        .chain(agent.allowed_playbook_refs.iter());
    for candidate in refs {
        let Some(urn) = candidate
            .resource_id
            .as_ref()
            .map(|id| id.urn.as_str())
            .filter(|urn| !urn.is_empty())
        else {
            continue;
        };
        if out.iter().any(|entry| entry.urn == urn) {
            continue;
        }
        let Some(pin) = pin_for(spec, urn) else {
            continue;
        };
        let Some(version) = pin.version.as_ref() else {
            continue;
        };
        let mut playbook =
            store.get_playbook_at_version(urn, version, owner_user_id, include_foreign_drafts)?;
        vam_spec::playbook::migrate_playbook_spec(&mut playbook);
        let when = playbook
            .activation
            .as_ref()
            .map(|activation| activation.when.trim().to_string())
            .unwrap_or_default();
        if when.is_empty() {
            continue;
        }
        out.push(AttachedPlaybook {
            urn: urn.to_string(),
            name: playbook_name(&playbook),
            when,
            default: urn == default_urn,
        });
    }
    Ok(out)
}

fn playbook_name(playbook: &PlaybookSpec) -> String {
    let name = playbook.display_name.trim();
    if name.is_empty() {
        playbook
            .resource_id
            .as_ref()
            .map(|id| id.urn.clone())
            .unwrap_or_default()
    } else {
        name.to_string()
    }
}

fn pin_for<'a>(spec: &'a RunSpec, urn: &str) -> Option<&'a ResourceRef> {
    spec.resolved_refs
        .iter()
        .find(|r| r.resource_id.as_ref().map(|id| id.urn.as_str()) == Some(urn))
}

/// Write the offered list into run inputs. An empty list writes nothing.
///
/// A caller-supplied value is always dropped: only the host decides what is offered.
pub fn insert_attached(inputs: Option<Struct>, attached: &[AttachedPlaybook]) -> Option<Struct> {
    let mut inputs = inputs;
    if let Some(fields) = inputs.as_mut() {
        fields.fields.remove(ATTACHED_PLAYBOOKS_FIELD);
    }
    if attached.is_empty() {
        return inputs;
    }
    let json = serde_json::to_string(attached).ok()?;
    let mut merged = inputs.unwrap_or_default();
    merged.fields.insert(
        ATTACHED_PLAYBOOKS_FIELD.to_string(),
        Value {
            kind: Some(Kind::StringValue(json)),
        },
    );
    Some(merged)
}

/// The playbooks the run offers, read back from the stored RunSpec.
pub fn run_attached(store: &SqliteWorkflowStore, run_id: &str) -> Vec<AttachedPlaybook> {
    let Ok((Some(spec_json), _, _)) = store.get_run_spec_context(run_id) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&spec_json) else {
        return Vec::new();
    };
    value
        .get("inputs")
        .and_then(|inputs| inputs.get(ATTACHED_PLAYBOOKS_FIELD))
        .and_then(|field| field.as_str())
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default()
}

/// True when the session's run offers playbooks, so `start_playbook` and the
/// step tools are in its catalog (the step tools stay inert until one is bound).
pub fn session_offers_playbooks(store: &SqliteWorkflowStore, session_id: &str) -> bool {
    match store.find_run_id_for_session(session_id) {
        Ok(Some(run_id)) => !run_attached(store, &run_id).is_empty(),
        _ => false,
    }
}

/// What `start_playbook` would do, decided before anything is written.
#[derive(Debug, Clone)]
pub struct StartPlan {
    pub urn: String,
    pub name: String,
    pub when: String,
    pub version: String,
    pub inputs: Struct,
    pub input_schema: serde_json::Value,
    pub has_inputs: bool,
    pub prepared: crate::run_context::PreparedPlaybookAdmission,
    /// A person must confirm before the playbook starts.
    pub ask: bool,
}

/// Check a `start_playbook` call and decide whether to ask. Every refusal is a
/// message the model can act on.
pub fn plan_start_playbook(
    store: &SqliteWorkflowStore,
    session_id: &str,
    run_id: &str,
    args: &Struct,
) -> Result<StartPlan, String> {
    if run_id.is_empty() {
        return Err("start_playbook needs a run. This session has none.".to_string());
    }
    let urn = match args.fields.get("urn").and_then(|v| v.kind.as_ref()) {
        Some(Kind::StringValue(text)) if !text.trim().is_empty() => text.trim().to_string(),
        _ => {
            return Err(
                "start_playbook requires a non-empty 'urn' string. Use one of the playbook urns listed in your instructions.".to_string(),
            )
        }
    };
    let attached = run_attached(store, run_id);
    let attached_list = || {
        attached
            .iter()
            .map(|p| p.urn.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Ok(Some(bound)) = store.get_playbook_run(run_id) {
        return Err(format!(
            "This task already follows playbook '{}'. A task runs one playbook. Continue with its steps, or finish it.",
            bound.urn
        ));
    }
    let Some(entry) = attached.iter().find(|p| p.urn == urn) else {
        return Err(if attached.is_empty() {
            format!("Playbook '{urn}' is not attached to this agent, and no playbooks are attached. Continue without a playbook.")
        } else {
            format!(
                "Playbook '{urn}' is not attached to this agent. Attached playbooks: {}. Pick one of these, or continue without a playbook.",
                attached_list()
            )
        });
    };
    let inputs = match args.fields.get("inputs").and_then(|v| v.kind.as_ref()) {
        None | Some(Kind::NullValue(_)) => Struct::default(),
        Some(Kind::StructValue(inputs)) => inputs.clone(),
        Some(_) => return Err(
            "start_playbook 'inputs' must be an object (use {} when the playbook takes no inputs)."
                .to_string(),
        ),
    };

    let (spec_json, _, _) = store
        .get_run_spec_context(run_id)
        .map_err(|e| e.to_string())?;
    let run_spec: RunSpec =
        vam_spec::encoding::json_to_proto("vam.v1.RunSpec", &spec_json.unwrap_or_default())
            .map_err(|e| format!("run spec unavailable: {e}"))?;
    let pin = pin_for(&run_spec, &urn).ok_or_else(|| {
        format!("Playbook '{urn}' is not pinned for this run. Continue without a playbook.")
    })?;
    let owner = store
        .session_owner_user_id(session_id)
        .unwrap_or_else(|_| crate::users::LOCAL_USER_ID.to_string());

    // Load first so a no-input playbook can refuse stray inputs by name.
    let version = pin
        .version
        .as_ref()
        .ok_or_else(|| format!("Playbook '{urn}' is not pinned to a version."))?;
    let playbook = store
        .get_playbook_at_version(&urn, version, &owner, true)
        .map_err(|e| e.to_string())?;
    let schema_json = playbook
        .input_schema
        .as_ref()
        .map(struct_to_json)
        .unwrap_or_else(|| serde_json::json!({}));
    let has_inputs = schema_has_properties(&schema_json);
    if !has_inputs && !inputs.fields.is_empty() {
        return Err(format!(
            "Playbook '{}' takes no inputs. Call start_playbook with just the urn (inputs omitted or {{}}).",
            entry.name
        ));
    }
    let prepared = crate::run_context::prepare_playbook_for_ref(
        store, pin, Some(&inputs), &owner, true,
    )
    .map_err(|e| match e {
        OrchestratorError::InvalidContract(msg) => format!(
            "Inputs do not match the input schema of '{}': {msg}. Fix them and call start_playbook again.",
            entry.name
        ),
        other => other.to_string(),
    })?;

    let ask = needs_ask(store, session_id, run_id, &playbook);
    Ok(StartPlan {
        urn,
        name: entry.name.clone(),
        when: entry.when.clone(),
        version: prepared.pin.version.clone(),
        inputs,
        input_schema: if has_inputs {
            schema_json
        } else {
            serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
        },
        has_inputs,
        prepared,
        ask,
    })
}

/// Ask unless the playbook is `auto` and nothing in it can change anything.
///
/// A bound or assisted step is mutating unless the session's catalog says its
/// tool is read-only (an unknown tool counts as mutating). An agentic step is
/// "open": the model picks the tool, so it is risky only when the run has seen
/// untrusted text.
fn needs_ask(
    store: &SqliteWorkflowStore,
    session_id: &str,
    run_id: &str,
    playbook: &PlaybookSpec,
) -> bool {
    let mode = playbook
        .activation
        .as_ref()
        .map(|a| ActivationMode::try_from(a.mode).unwrap_or(ActivationMode::Ask))
        .unwrap_or(ActivationMode::Ask);
    if mode != ActivationMode::Auto {
        return true;
    }
    let catalog = store.get_tool_catalog(session_id).unwrap_or_default();
    let mut mutating = false;
    let mut open = false;
    for step in &playbook.steps {
        if StepKind::try_from(step.kind).unwrap_or(StepKind::Task) == StepKind::Gate {
            continue;
        }
        let bound_tool = step
            .binding
            .as_ref()
            .map(|b| b.tool_name.as_str())
            .filter(|name| !name.is_empty());
        match (
            StepMode::try_from(step.mode).unwrap_or(StepMode::Unspecified),
            bound_tool,
        ) {
            (StepMode::Agentic, _) | (_, None) => open = true,
            (_, Some(tool)) => {
                let read_only = catalog.iter().any(|entry| {
                    entry.tool_name == tool && entry.side_effect == SideEffectClass::ReadOnly as i32
                });
                if !read_only {
                    mutating = true;
                }
            }
        }
    }
    mutating || (open && crate::host_tools::run_untrusted_seen(store, run_id))
}

/// Bind the planned playbook to the current run. Returns the rendered procedure
/// the guest shows the model.
pub fn bind_playbook(
    store: &SqliteWorkflowStore,
    run_id: &str,
    plan: &StartPlan,
) -> Result<String, String> {
    let guidance_only = plan.prepared.is_guidance_only();
    if !guidance_only {
        store
            .init_playbook_run(run_id, &plan.prepared.pin, &plan.prepared.spec)
            .map_err(|err| match err {
                OrchestratorError::Conflict(_) => {
                    "This task already follows a playbook. A task runs one playbook. Continue with its steps, or finish it.".to_string()
                }
                other => other.to_string(),
            })?;
        store
            .set_run_input_string(
                run_id,
                crate::run_context::PLAYBOOK_PROCEDURE_FIELD,
                &plan.prepared.procedure,
            )
            .map_err(|e| e.to_string())?;
        // The validated start inputs, so `{{inputs.x}}` in gates and bound
        // arguments resolves for the rest of the run.
        store
            .set_run_input_json(
                run_id,
                crate::run_context::PLAYBOOK_INPUTS_FIELD,
                struct_to_json(&plan.inputs),
            )
            .map_err(|e| e.to_string())?;
    }
    let payload = serde_json::json!({
        "urn": plan.urn,
        "version": plan.version,
        "asked": plan.ask,
        "guidance": guidance_only,
    })
    .to_string();
    let _ = store.append_outbox_event(run_id, EVENT_PLAYBOOK_ACTIVATED, &payload);
    if guidance_only {
        // Nothing is bound or tracked: the guidance is added to the conversation
        // by this result, and the run ends normally.
        return Ok(format!(
            "{}'{}'. Follow this guidance for the rest of the task:\n\n{}",
            vam_spec::playbook::PLAYBOOK_GUIDANCE_PREFIX,
            plan.name,
            plan.prepared.procedure
        ));
    }
    let status = crate::playbook_flight::status_block(store, run_id).unwrap_or_default();
    Ok(format!(
        "{}'{}'. Follow the procedure below: call run_step for a bound step, do the work and call step_complete for an agentic step, and wait for the user on a gate. Never invent a step's outcome. {}\n\n{}\n\n{}",
        vam_spec::playbook::PLAYBOOK_STARTED_PREFIX,
        plan.name,
        vam_spec::playbook::PLAYBOOK_GATE_RULE,
        plan.prepared.procedure,
        status
    ))
}

fn schema_has_properties(schema: &serde_json::Value) -> bool {
    schema
        .get("properties")
        .and_then(|p| p.as_object())
        .is_some_and(|p| !p.is_empty())
}

pub(crate) fn struct_to_json(value: &Struct) -> serde_json::Value {
    serde_json::Value::Object(
        value
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), value_to_json(v)))
            .collect(),
    )
}

fn value_to_json(value: &Value) -> serde_json::Value {
    match value.kind.as_ref() {
        None | Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(Kind::NumberValue(n)) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(Kind::StructValue(s)) => struct_to_json(s),
        Some(Kind::ListValue(l)) => {
            serde_json::Value::Array(l.values.iter().map(value_to_json).collect())
        }
    }
}

/// A JSON object as a protobuf `Struct`. `None` when `value` is not an object.
pub(crate) fn json_to_struct(value: &serde_json::Value) -> Option<Struct> {
    vam_spec::encoding::json_to_proto("google.protobuf.Struct", &value.to_string()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{resolve_session_policy, SessionSecurityContext};
    use crate::session::{builtin_tool_catalog, ToolCatalogEntry};
    use crate::users::LOCAL_USER_ID;
    use std::sync::Arc;
    use vam_inference::SessionBudget;
    use vam_security::PolicyMode;
    use vam_spec::v1::agent_spec::InstructionSource;
    use vam_spec::v1::tool_intent_response::Disposition;
    use vam_spec::v1::{
        ExecuteHostToolRequest, ExecutionLease, PlaybookActivation, PlaybookStep, ResourceId,
        ToolIntentRequest,
    };

    const AGENT: &str = "urn:vam:agent:local.vam.dev/acme/act-agent";
    const PB: &str = "urn:vam:playbook:local.vam.dev/acme/act-pb";
    const PB_DEFAULT: &str = "urn:vam:playbook:local.vam.dev/acme/act-default";
    const PB_OTHER: &str = "urn:vam:playbook:local.vam.dev/acme/act-other";
    const SESSION: &str = "sess-act";
    const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-act";

    fn step(id: &str, mode: StepMode, tool: Option<&str>) -> PlaybookStep {
        PlaybookStep {
            id: id.to_string(),
            kind: StepKind::Task as i32,
            mode: mode as i32,
            instruction: format!("Do {id} for {{{{inputs.title}}}}."),
            binding: tool.map(|name| vam_spec::v1::StepBinding {
                tool_name: name.to_string(),
                arguments: Some(Struct::default()),
            }),
            ..Default::default()
        }
    }

    fn playbook(
        urn: &str,
        name: &str,
        mode: ActivationMode,
        with_inputs: bool,
        steps: Vec<PlaybookStep>,
    ) -> PlaybookSpec {
        PlaybookSpec {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: name.to_string(),
            description: "test".to_string(),
            activation: Some(PlaybookActivation {
                when: format!("Use when the user wants {name}."),
                examples: vec![],
                mode: mode as i32,
            }),
            input_schema: with_inputs.then(|| {
                json_to_struct(&serde_json::json!({
                    "type": "object",
                    "properties": {"title": {"type": "string"}},
                    "required": ["title"],
                    "additionalProperties": false
                }))
                .expect("schema")
            }),
            steps,
            ..Default::default()
        }
    }

    fn ref_to(urn: &str) -> ResourceRef {
        ResourceRef {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            version: None,
            digest: None,
            locators: vec![],
        }
    }

    fn host_entry(name: &str, side_effect: i32) -> ToolCatalogEntry {
        ToolCatalogEntry {
            tool_name: name.to_string(),
            command: "host".to_string(),
            entrypoint: name.to_string(),
            digest: vam_spec::validation::host_tool_digest(name),
            side_effect,
            skill_name: String::new(),
            approval_required: false,
        }
    }

    fn lease() -> ExecutionLease {
        ExecutionLease {
            run_id: Some(ResourceId {
                urn: RUN.to_string(),
            }),
            step_id: "step-1".to_string(),
            attempt_id: Some(ResourceId {
                urn: "urn:vam:attempt:local.vam.dev/acme/act".to_string(),
            }),
            lease_id: Some(ResourceId {
                urn: "urn:vam:lease:local.vam.dev/acme/act".to_string(),
            }),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: Some(prost_types::Timestamp {
                seconds: 4_102_444_800,
                nanos: 0,
            }),
            idempotency_key: "idem-act".to_string(),
        }
    }

    struct Fx {
        store: SqliteWorkflowStore,
        ctx: SessionSecurityContext,
    }

    /// An agent that attaches `playbooks` (the first is the default), a run that
    /// offers them, a session with the playbook tools in its catalog, and the
    /// Cedar policy the host composes for that session.
    fn fixture(playbooks: Vec<PlaybookSpec>, extra_released: Vec<PlaybookSpec>) -> Fx {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        for pb in playbooks.iter().chain(extra_released.iter()) {
            store
                .put_playbook_with_owner(pb, LOCAL_USER_ID)
                .expect("release playbook");
        }
        let urns: Vec<String> = playbooks
            .iter()
            .filter_map(|pb| pb.resource_id.as_ref().map(|id| id.urn.clone()))
            .collect();
        let agent = AgentSpec {
            resource_id: Some(ResourceId {
                urn: AGENT.to_string(),
            }),
            display_name: "Act".to_string(),
            description: "test".to_string(),
            role: "tester".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions("go".into())),
            // Not widening: allow_any_playbook must not offer PB_OTHER.
            allow_any_playbook: true,
            default_playbook_ref: urns.first().map(|urn| ref_to(urn)),
            allowed_playbook_refs: urns.iter().skip(1).map(|urn| ref_to(urn)).collect(),
            ..Default::default()
        };
        store
            .put_agent_released_with_owner(&agent, LOCAL_USER_ID)
            .expect("agent");
        let mut spec = RunSpec {
            run_id: Some(ResourceId {
                urn: RUN.to_string(),
            }),
            workspace_ref_uri: "volume://assistant".to_string(),
            ..Default::default()
        };
        crate::catalog_resolve::record_resolved_refs(
            &store,
            &mut spec,
            &agent,
            LOCAL_USER_ID,
            false,
        )
        .expect("resolve");
        let attached =
            build_attached(&store, &spec, &agent, LOCAL_USER_ID, false).expect("attached");
        spec.inputs = insert_attached(None, &attached);
        let spec_json =
            vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &spec).expect("run spec json");
        store
            .create_session(SESSION, None, "vm-test", "ws-test", "storage-default")
            .expect("session");
        store
            .persist_run_spec(
                RUN,
                &spec_json,
                Some(SESSION),
                AGENT,
                None,
                "User::\"tester\"",
            )
            .expect("run");
        let mut catalog = builtin_tool_catalog();
        catalog.push(host_entry(START_PLAYBOOK_TOOL, 2));
        catalog.push(host_entry("step_complete", 2));
        catalog.push(host_entry("run_step", 2));
        catalog.push(host_entry("check_playbook_finish", 1));
        store
            .record_tool_catalog(SESSION, &catalog)
            .expect("catalog");
        store.issue_lease(&lease()).expect("lease");
        let cedar = resolve_session_policy(&store, SESSION, Some(AGENT))
            .expect("policy")
            .cedar;
        let ctx = SessionSecurityContext::new(
            SESSION.to_string(),
            AGENT.to_string(),
            &cedar,
            PolicyMode::Enforce,
            1,
            ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]),
            None,
            Arc::new(SessionBudget::new(None)),
        )
        .expect("ctx");
        Fx { store, ctx }
    }

    fn start_args(urn: &str, inputs: Option<serde_json::Value>) -> Struct {
        let mut json = serde_json::json!({ "urn": urn });
        if let Some(inputs) = inputs {
            json["inputs"] = inputs;
        }
        json_to_struct(&json).expect("args")
    }

    fn intent(fx: &Fx, tool: &str, args: Struct) -> vam_spec::v1::ToolIntentResponse {
        fx.ctx.handle_tool_intent_request(
            &fx.store,
            ToolIntentRequest {
                lease: Some(lease()),
                tool_name: tool.to_string(),
                arguments: Some(args),
                arguments_digest: None,
                script_digest: Some(vam_spec::v1::Digest {
                    algorithm: 1,
                    hex_hash: vam_spec::validation::host_tool_digest(tool),
                }),
                call_id: String::new(),
            },
        )
    }

    async fn execute(fx: &Fx, tool: &str, args: Struct) -> String {
        let resp = intent(fx, tool, args.clone());
        assert_eq!(
            resp.disposition,
            Disposition::Authorized as i32,
            "{:?}",
            resp.error
        );
        let registry = crate::host_tools::HostToolRegistry::with_test_adapters();
        let skills_dir = std::env::temp_dir().join(format!("vam-act-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&skills_dir);
        let out = fx
            .ctx
            .handle_execute_host_tool(
                &fx.store,
                None,
                &registry,
                &skills_dir,
                ExecuteHostToolRequest {
                    lease: Some(lease()),
                    tool_name: tool.to_string(),
                    arguments: Some(args),
                    capability_token: resp.capability_token,
                    call_id: String::new(),
                },
            )
            .await;
        out.text_result
    }

    fn plan(fx: &Fx, args: &Struct) -> Result<StartPlan, String> {
        plan_start_playbook(&fx.store, SESSION, RUN, args)
    }

    fn one_task(mode: ActivationMode, with_inputs: bool, tool: Option<&str>) -> Vec<PlaybookSpec> {
        let step_mode = if tool.is_some() {
            StepMode::Bound
        } else {
            StepMode::Agentic
        };
        vec![playbook(
            PB,
            "a pull request",
            mode,
            with_inputs,
            vec![step("draft", step_mode, tool)],
        )]
    }

    #[test]
    fn the_default_is_listed_first_and_allow_any_playbook_does_not_widen_the_offer() {
        let fx = fixture(
            vec![
                playbook(
                    PB_DEFAULT,
                    "the main job",
                    ActivationMode::Ask,
                    false,
                    vec![step("a", StepMode::Agentic, None)],
                ),
                playbook(
                    PB,
                    "a pull request",
                    ActivationMode::Ask,
                    false,
                    vec![step("a", StepMode::Agentic, None)],
                ),
            ],
            // Released and findable, but not attached to the agent.
            vec![playbook(
                PB_OTHER,
                "something else",
                ActivationMode::Ask,
                false,
                vec![step("a", StepMode::Agentic, None)],
            )],
        );
        let offered = run_attached(&fx.store, RUN);
        let urns: Vec<&str> = offered.iter().map(|p| p.urn.as_str()).collect();
        assert_eq!(urns, vec![PB_DEFAULT, PB], "allow_any_playbook is true");
        assert!(offered[0].default && !offered[1].default);
        assert_eq!(offered[0].when, "Use when the user wants the main job.");
        assert!(session_offers_playbooks(&fx.store, SESSION));
    }

    #[test]
    fn a_caller_cannot_supply_the_offered_list() {
        let mut inputs = Struct::default();
        inputs.fields.insert(
            ATTACHED_PLAYBOOKS_FIELD.to_string(),
            Value {
                kind: Some(Kind::StringValue("[{\"urn\":\"x\"}]".to_string())),
            },
        );
        let cleaned = insert_attached(Some(inputs), &[]).expect("inputs");
        assert!(!cleaned.fields.contains_key(ATTACHED_PLAYBOOKS_FIELD));
    }

    #[tokio::test]
    async fn a_start_binds_the_playbook_with_inputs_and_without() {
        // With inputs.
        let fx = fixture(one_task(ActivationMode::Ask, true, None), vec![]);
        let args = start_args(PB, Some(serde_json::json!({"title": "Fix the bug"})));
        let planned = plan(&fx, &args).expect("plan");
        assert!(planned.has_inputs && planned.ask);
        let text = bind_playbook(&fx.store, RUN, &planned).expect("bind");
        assert!(
            text.starts_with(vam_spec::playbook::PLAYBOOK_STARTED_PREFIX),
            "{text}"
        );
        assert!(text.contains("Do draft for Fix the bug."), "{text}");
        let bound = fx.store.get_playbook_run(RUN).expect("row").expect("bound");
        assert_eq!(bound.urn, PB);
        let steps = fx.store.list_playbook_steps(RUN).expect("steps");
        assert_eq!(steps.len(), 1);
        // The procedure survives a resume: it is in the stored run inputs.
        let (spec_json, _, _) = fx.store.get_run_spec_context(RUN).expect("ctx");
        let stored: serde_json::Value =
            serde_json::from_str(&spec_json.expect("json")).expect("json");
        assert!(
            stored["inputs"][crate::run_context::PLAYBOOK_PROCEDURE_FIELD]
                .as_str()
                .is_some_and(|p| p.contains("Fix the bug"))
        );
        let events = fx.store.read_outbox_for_run(RUN, 0).expect("events");
        assert!(events
            .iter()
            .any(|e| e.event_type == EVENT_PLAYBOOK_ACTIVATED));

        // Without inputs: `inputs` omitted, then `{}`.
        for inputs in [None, Some(serde_json::json!({}))] {
            let fx = fixture(one_task(ActivationMode::Ask, false, None), vec![]);
            let planned = plan(&fx, &start_args(PB, inputs)).expect("no-input plan");
            assert!(!planned.has_inputs);
            assert_eq!(
                planned.input_schema,
                serde_json::json!({"type":"object","properties":{},"additionalProperties":false})
            );
            bind_playbook(&fx.store, RUN, &planned).expect("bind");
            assert!(fx.store.get_playbook_run(RUN).expect("row").is_some());
        }
    }

    #[test]
    fn refusals_name_the_problem_and_the_next_step() {
        let fx = fixture(
            one_task(ActivationMode::Ask, true, None),
            vec![playbook(
                PB_OTHER,
                "something else",
                ActivationMode::Ask,
                false,
                vec![step("a", StepMode::Agentic, None)],
            )],
        );
        let good = serde_json::json!({"title": "x"});

        let err = plan(&fx, &start_args(PB_OTHER, None)).expect_err("unattached");
        assert!(err.contains("not attached to this agent"), "{err}");
        assert!(err.contains(PB), "lists what is attached: {err}");

        let err = plan(&fx, &Struct::default()).expect_err("no urn");
        assert!(err.contains("'urn'"), "{err}");

        let err = plan(&fx, &start_args(PB, Some(serde_json::json!({})))).expect_err("missing");
        assert!(err.contains("Inputs do not match"), "{err}");
        assert!(err.contains("call start_playbook again"), "{err}");
        let err =
            plan(&fx, &start_args(PB, Some(serde_json::json!({"title": 3})))).expect_err("type");
        assert!(err.contains("Inputs do not match"), "{err}");
        let err = plan(&fx, &start_args(PB, Some(serde_json::json!("text")))).expect_err("shape");
        assert!(err.contains("must be an object"), "{err}");

        let planned = plan(&fx, &start_args(PB, Some(good.clone()))).expect("plan");
        bind_playbook(&fx.store, RUN, &planned).expect("bind");
        let err = plan(&fx, &start_args(PB, Some(good))).expect_err("second");
        assert!(err.contains("already follows playbook"), "{err}");
        assert!(err.contains("one playbook"), "{err}");
    }

    #[test]
    fn a_playbook_with_no_inputs_refuses_stray_inputs_by_name() {
        let fx = fixture(one_task(ActivationMode::Ask, false, None), vec![]);
        let err = plan(&fx, &start_args(PB, Some(serde_json::json!({"x": 1})))).expect_err("stray");
        assert!(err.contains("takes no inputs"), "{err}");
        assert!(err.contains("a pull request"), "{err}");
    }

    #[test]
    fn ask_is_the_default_and_auto_asks_when_a_step_can_change_something() {
        // Ask mode always asks.
        let fx = fixture(
            one_task(ActivationMode::Ask, false, Some("read_file")),
            vec![],
        );
        assert!(plan(&fx, &start_args(PB, None)).expect("plan").ask);
        // Unspecified mode is ask.
        let fx = fixture(one_task(ActivationMode::Unspecified, false, None), vec![]);
        assert!(plan(&fx, &start_args(PB, None)).expect("plan").ask);
        // Auto with only a read-only bound step starts without asking.
        let fx = fixture(
            one_task(ActivationMode::Auto, false, Some("read_file")),
            vec![],
        );
        assert!(!plan(&fx, &start_args(PB, None)).expect("plan").ask);
        // Auto with a bound tool the catalog does not know as read-only asks.
        let fx = fixture(
            one_task(ActivationMode::Auto, false, Some("notion__create_page")),
            vec![],
        );
        assert!(plan(&fx, &start_args(PB, None)).expect("plan").ask);
        // Auto with only an agentic step: no ask on a trusted run ...
        let fx = fixture(one_task(ActivationMode::Auto, false, None), vec![]);
        assert!(!plan(&fx, &start_args(PB, None)).expect("plan").ask);
        // ... and an ask once the run has seen untrusted text. Sticky.
        fx.store
            .append_outbox_event(
                RUN,
                crate::host_tools::EVENT_VENDOR_TEXT_FLAGGED,
                "{\"turn\":1}",
            )
            .expect("flag");
        assert!(plan(&fx, &start_args(PB, None)).expect("plan").ask);
        // A read-only auto playbook still starts on an untrusted run.
        let fx = fixture(
            one_task(ActivationMode::Auto, false, Some("read_file")),
            vec![],
        );
        fx.store
            .append_outbox_event(
                RUN,
                crate::host_tools::EVENT_VENDOR_TEXT_FLAGGED,
                "{\"turn\":1}",
            )
            .expect("flag");
        assert!(!plan(&fx, &start_args(PB, None)).expect("plan").ask);
    }

    #[test]
    fn the_overlay_and_the_step_tools_follow_the_offer() {
        let fx = fixture(one_task(ActivationMode::Ask, false, None), vec![]);
        let cedar = crate::security::derive_playbook_overlay(&fx.store, SESSION).expect("overlay");
        for tool in [
            "start_playbook",
            "step_complete",
            "run_step",
            "check_playbook_finish",
        ] {
            assert!(cedar.contains(tool), "offers: {tool} missing from {cedar}");
        }
        let advertised = fx.ctx.advertised_tool_names(&fx.store);
        assert!(advertised.contains(&"start_playbook".to_string()));
        // Once bound, the start permit is gone and the step tools stay.
        let planned = plan(&fx, &start_args(PB, None)).expect("plan");
        bind_playbook(&fx.store, RUN, &planned).expect("bind");
        let cedar = crate::security::derive_playbook_overlay(&fx.store, SESSION).expect("overlay");
        assert!(!cedar.contains("start_playbook"), "{cedar}");
        assert!(cedar.contains("step_complete"));
        // No offer and nothing bound: nothing is permitted.
        let none = fixture(vec![], vec![]);
        assert!(
            crate::security::derive_playbook_overlay(&none.store, SESSION)
                .expect("overlay")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_step_tools_are_inert_until_a_playbook_is_bound() {
        let fx = fixture(
            one_task(ActivationMode::Auto, false, Some("read_file")),
            vec![],
        );
        let step_args = json_to_struct(&serde_json::json!({"step_id": "draft", "outcome": "done"}))
            .expect("args");
        let text = execute(&fx, "step_complete", step_args.clone()).await;
        assert!(text.starts_with("tool_error:"), "{text}");
        assert!(text.contains("Call start_playbook first."), "{text}");
        // Auto, read-only: the intent allows it, the host binds it.
        let text = execute(&fx, START_PLAYBOOK_TOOL, start_args(PB, None)).await;
        assert!(
            text.starts_with(vam_spec::playbook::PLAYBOOK_STARTED_PREFIX),
            "{text}"
        );
        let text = execute(&fx, "step_complete", step_args).await;
        assert!(
            !text.starts_with("tool_error:"),
            "now it records the step: {text}"
        );
    }

    fn pending_row(fx: &Fx) -> (String, String, String, String) {
        let connection = fx.store.connection.lock().expect("lock");
        connection
            .query_row(
                "SELECT request_id, requested_args_json, amendable_schema_json, approval_message
                 FROM hitl_pending WHERE run_id = ?1",
                [RUN],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("a pending approval")
    }

    #[tokio::test]
    async fn ask_parks_with_the_inputs_prefilled_and_an_amended_approval_binds_the_amended_inputs()
    {
        let fx = fixture(one_task(ActivationMode::Ask, true, None), vec![]);
        let args = start_args(PB, Some(serde_json::json!({"title": "Fix the bug"})));
        let parked = intent(&fx, START_PLAYBOOK_TOOL, args);
        assert_eq!(
            parked.disposition,
            Disposition::Suspended as i32,
            "{:?}",
            parked.error
        );
        assert!(
            fx.store.get_playbook_run(RUN).expect("row").is_none(),
            "nothing bound yet"
        );

        let (request_id, requested, schema, message) = pending_row(&fx);
        let requested: serde_json::Value = serde_json::from_str(&requested).expect("json");
        assert_eq!(
            requested,
            serde_json::json!({"urn": PB, "inputs": {"title": "Fix the bug"}})
        );
        let schema: serde_json::Value = serde_json::from_str(&schema).expect("json");
        assert_eq!(schema["title"], "a pull request");
        assert_eq!(
            schema["description"],
            "Use when the user wants a pull request."
        );
        assert_eq!(schema["x-vam-playbook"]["urn"], PB);
        assert_eq!(schema["x-vam-playbook"]["has_inputs"], true);
        assert_eq!(schema["properties"]["urn"]["const"], PB);
        assert_eq!(schema["properties"]["inputs"]["required"][0], "title");
        assert!(
            message.contains("Start playbook 'a pull request'?"),
            "{message}"
        );
        let target_locked: (i64, String) = {
            let connection = fx.store.connection.lock().expect("lock");
            connection
                .query_row(
                    "SELECT always_offered, locked_reason FROM hitl_pending WHERE request_id = ?1",
                    [&request_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("row")
        };
        assert_eq!(target_locked.0, 0, "Always is never offered for a start");

        // The reviewer edits the title. The amend is checked against the schema.
        let amended = serde_json::json!({"urn": PB, "inputs": {"title": "Fix the other bug"}});
        crate::hitl::validate_amended_args(&amended, &schema).expect("keys fit");
        crate::hitl::validate_start_playbook_amend(
            &serde_json::to_string(&requested).expect("json"),
            &amended,
            &schema,
        )
        .expect("amend ok");
        // The decide path records a grant for the amended arguments' digest.
        let amended_struct = json_to_struct(&amended).expect("struct");
        let digest = vam_spec::canonical_args_digest(Some(&amended_struct)).expect("digest");
        {
            let connection = fx.store.connection.lock().expect("lock");
            connection
                .execute(
                    "INSERT INTO hitl_grants (session_id, tool_name, args_digest) VALUES (?1, ?2, ?3)",
                    rusqlite::params![SESSION, START_PLAYBOOK_TOOL, digest],
                )
                .expect("grant");
        }
        let text = execute(&fx, START_PLAYBOOK_TOOL, amended_struct).await;
        assert!(text.contains("Do draft for Fix the other bug."), "{text}");
        assert!(
            !text.contains("Fix the bug"),
            "the original inputs are not used: {text}"
        );
    }

    #[test]
    fn the_ask_payload_for_a_playbook_without_inputs_says_there_is_no_form() {
        let fx = fixture(one_task(ActivationMode::Ask, false, None), vec![]);
        let parked = intent(&fx, START_PLAYBOOK_TOOL, start_args(PB, None));
        assert_eq!(
            parked.disposition,
            Disposition::Suspended as i32,
            "{:?}",
            parked.error
        );
        let (_, requested, schema, _) = pending_row(&fx);
        let requested: serde_json::Value = serde_json::from_str(&requested).expect("json");
        assert_eq!(requested, serde_json::json!({"urn": PB, "inputs": {}}));
        let schema: serde_json::Value = serde_json::from_str(&schema).expect("json");
        assert_eq!(schema["x-vam-playbook"]["has_inputs"], false);
        assert_eq!(
            schema["properties"]["inputs"],
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false})
        );
        // Confirming without a form amends to the same empty inputs; a stray value is refused.
        let same = serde_json::json!({"urn": PB, "inputs": {}});
        crate::hitl::validate_start_playbook_amend(
            &serde_json::to_string(&requested).expect("json"),
            &same,
            &schema,
        )
        .expect("empty inputs ok");
        let stray = serde_json::json!({"urn": PB, "inputs": {"x": 1}});
        assert!(crate::hitl::validate_start_playbook_amend(
            &serde_json::to_string(&requested).expect("json"),
            &stray,
            &schema,
        )
        .is_err());
    }

    #[test]
    fn a_refused_start_is_denied_at_intent_with_the_message() {
        let fx = fixture(one_task(ActivationMode::Ask, true, None), vec![]);
        let denied = intent(
            &fx,
            START_PLAYBOOK_TOOL,
            start_args(PB, Some(serde_json::json!({}))),
        );
        assert_eq!(denied.disposition, Disposition::Denied as i32);
        let message = denied.error.expect("error").message;
        assert!(message.contains("Inputs do not match"), "{message}");
        assert!(fx.store.get_playbook_run(RUN).expect("row").is_none());
        // No approval was parked for a start that could not succeed.
        let connection = fx.store.connection.lock().expect("lock");
        let parked: i64 = connection
            .query_row("SELECT COUNT(*) FROM hitl_pending", [], |row| row.get(0))
            .expect("count");
        assert_eq!(parked, 0);
    }

    #[test]
    fn amend_checks_reject_a_swapped_playbook_and_invalid_inputs() {
        let fx = fixture(one_task(ActivationMode::Ask, true, None), vec![]);
        let _ = intent(
            &fx,
            START_PLAYBOOK_TOOL,
            start_args(PB, Some(serde_json::json!({"title": "x"}))),
        );
        let (_, requested, schema, _) = pending_row(&fx);
        let schema: serde_json::Value = serde_json::from_str(&schema).expect("json");
        let swapped = serde_json::json!({"urn": PB_OTHER, "inputs": {"title": "x"}});
        let err = crate::hitl::validate_start_playbook_amend(&requested, &swapped, &schema)
            .expect_err("swap");
        assert!(err.to_string().contains("urn cannot be changed"), "{err}");
        let bad = serde_json::json!({"urn": PB, "inputs": {"title": 7}});
        assert!(crate::hitl::validate_start_playbook_amend(&requested, &bad, &schema).is_err());
        let missing = serde_json::json!({"urn": PB, "inputs": {}});
        assert!(crate::hitl::validate_start_playbook_amend(&requested, &missing, &schema).is_err());
    }

    // ---- Playbooks in flight (AR-0.11) -------------------------------------

    struct SuppliersAdapter;

    impl crate::host_tools::HostToolAdapter for SuppliersAdapter {
        fn adapter_id(&self) -> &str {
            "suppliers"
        }
        fn invoke(
            &self,
            ctx: &crate::host_tools::HostToolContext<'_>,
        ) -> Result<crate::host_tools::HostToolOutput, String> {
            Ok(crate::host_tools::HostToolOutput {
                text: serde_json::json!({
                    "suppliers": ["SUP-1", "SUP-2"],
                    "searched": struct_to_json(ctx.args)["query"],
                })
                .to_string(),
                artifact: None,
            })
        }
    }

    fn flight_registry() -> crate::host_tools::HostToolRegistry {
        let mut registry = crate::host_tools::HostToolRegistry::with_test_adapters();
        registry.register(Arc::new(SuppliersAdapter));
        registry
    }

    /// The fixture with extra host tools `(name, adapter, side effect)` in the
    /// catalog and a Cedar permit for each.
    fn with_tools(fx: Fx, tools: &[(&str, &str, i32)]) -> Fx {
        let mut catalog = builtin_tool_catalog();
        catalog.push(host_entry(START_PLAYBOOK_TOOL, 2));
        catalog.push(host_entry("step_complete", 2));
        catalog.push(host_entry("run_step", 2));
        catalog.push(host_entry("check_playbook_finish", 1));
        let mut cedar = resolve_session_policy(&fx.store, SESSION, Some(AGENT))
            .expect("policy")
            .cedar;
        for (name, adapter, side_effect) in tools {
            let mut entry = host_entry(adapter, *side_effect);
            entry.tool_name = (*name).to_string();
            catalog.push(entry);
            cedar.push_str(&format!(
                "\npermit (principal, action == Action::\"ExecuteTool\", resource == Tool::\"{name}\");"
            ));
        }
        fx.store
            .record_tool_catalog(SESSION, &catalog)
            .expect("catalog");
        let ctx = SessionSecurityContext::new(
            SESSION.to_string(),
            AGENT.to_string(),
            &cedar,
            PolicyMode::Enforce,
            1,
            ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]),
            None,
            Arc::new(SessionBudget::new(None)),
        )
        .expect("ctx");
        Fx {
            store: fx.store,
            ctx,
        }
    }

    async fn execute_with(
        fx: &Fx,
        registry: &crate::host_tools::HostToolRegistry,
        tool: &str,
        args: Struct,
    ) -> String {
        let resp = intent(fx, tool, args.clone());
        assert_eq!(
            resp.disposition,
            Disposition::Authorized as i32,
            "{:?}",
            resp.error
        );
        let skills_dir = std::env::temp_dir().join(format!("vam-flight-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&skills_dir);
        fx.ctx
            .handle_execute_host_tool(
                &fx.store,
                None,
                registry,
                &skills_dir,
                ExecuteHostToolRequest {
                    lease: Some(lease()),
                    tool_name: tool.to_string(),
                    arguments: Some(args),
                    capability_token: resp.capability_token,
                    call_id: String::new(),
                },
            )
            .await
            .text_result
    }

    fn run_step_args(step_id: &str) -> Struct {
        json_to_struct(&serde_json::json!({ "step_id": step_id })).expect("args")
    }

    fn decide_gate(fx: &Fx, step: &str, response: serde_json::Value) {
        fx.store
            .decide_approval(
                &ResourceId {
                    urn: format!("{RUN}/gate/{step}"),
                },
                &ResourceId {
                    urn: format!("urn:vam:token:local.vam.dev/acme/{step}"),
                },
                "SUBMITTED",
                "tester",
                LOCAL_USER_ID,
                None,
                None,
                None,
                Some(&response.to_string()),
            )
            .unwrap_or_else(|e| panic!("gate {step}: {e}"));
    }

    fn gate_schema(fx: &Fx, step: &str) -> serde_json::Value {
        let connection = fx.store.connection.lock().expect("lock");
        let json: String = connection
            .query_row(
                "SELECT amendable_schema_json FROM approvals WHERE run_id = ?1 AND step_id = ?2",
                [RUN, step],
                |row| row.get(0),
            )
            .expect("gate approval");
        serde_json::from_str(&json).expect("schema")
    }

    fn grant(fx: &Fx, digest: &str) {
        let connection = fx.store.connection.lock().expect("lock");
        connection
            .execute(
                "INSERT INTO hitl_grants (session_id, tool_name, args_digest) VALUES (?1, 'run_step', ?2)",
                rusqlite::params![SESSION, digest],
            )
            .expect("grant");
    }

    fn bind(fx: &Fx, urn: &str, inputs: Option<serde_json::Value>) -> String {
        let planned = plan(fx, &start_args(urn, inputs)).expect("plan");
        bind_playbook(&fx.store, RUN, &planned).expect("bind")
    }

    fn oracle() -> PlaybookSpec {
        vam_spec::encoding::json_to_proto(
            "vam.v1.PlaybookSpec",
            include_str!("../../../examples/oracle-purchase-requisition/playbook.json"),
        )
        .expect("oracle decodes")
    }

    const ORACLE: &str = "urn:vam:playbook:local.vam.dev/vam/oracle-purchase-requisition";

    fn oracle_fixture() -> Fx {
        with_tools(
            fixture(vec![oracle()], vec![]),
            &[
                ("oracle_search_suppliers", "suppliers", 1),
                ("oracle_create_purchase_requisition", "echo", 3),
            ],
        )
    }

    #[tokio::test]
    async fn the_oracle_flow_reaches_create_pr_with_arguments_from_the_captured_values() {
        let fx = oracle_fixture();
        let registry = flight_registry();
        let started = bind(&fx, ORACLE, None);
        assert!(started.starts_with(vam_spec::playbook::PLAYBOOK_STARTED_PREFIX));
        assert!(started.contains(vam_spec::playbook::PLAYBOOK_GATE_RULE));
        assert!(started.contains("[Playbook status: Oracle purchase requisition]"));
        assert!(started.contains("Waiting for the user"));

        decide_gate(&fx, "mode", serde_json::json!({"option": "guided"}));
        // G5: the default templated from the invoice path is dropped on the guided path.
        let schema = gate_schema(&fx, "supplier-name");
        assert!(
            schema["properties"]["name"].get("default").is_none(),
            "{schema}"
        );
        decide_gate(&fx, "supplier-name", serde_json::json!({"name": "Acme"}));

        // The model's call names only the step; a guest-sent argument is ignored (e).
        let guest =
            json_to_struct(&serde_json::json!({"step_id": "find-suppliers", "query": "EVIL"}))
                .expect("args");
        let found = execute_with(&fx, &registry, "run_step", guest).await;
        let found: serde_json::Value = serde_json::from_str(&found).expect("json result");
        assert!(
            found["result"]
                .as_str()
                .unwrap_or("")
                .contains("\"searched\":\"Acme\""),
            "{found}"
        );
        assert!(!found.to_string().contains("EVIL"));
        let status = found["status"].as_str().expect("status block");
        assert!(status.contains("Done: mode=guided"), "{status}");
        assert!(status.contains("Captured from tools:"), "{status}");
        assert!(
            status.contains("<<<"),
            "bound output stays fenced: {status}"
        );

        // choices_from filled the enum from the captured tool output.
        let schema = gate_schema(&fx, "pick-supplier");
        assert_eq!(
            schema["properties"]["supplier_id"]["enum"],
            serde_json::json!(["SUP-1", "SUP-2"])
        );
        decide_gate(
            &fx,
            "pick-supplier",
            serde_json::json!({"supplier_id": "SUP-2"}),
        );
        decide_gate(
            &fx,
            "details",
            serde_json::json!({"items": "10 laptops", "cost_centre": "CC-42", "need_by": "2026-12-01"}),
        );
        let question: String = {
            let connection = fx.store.connection.lock().expect("lock");
            connection
                .query_row(
                    "SELECT approval_message FROM approvals WHERE run_id = ?1 AND step_id = 'confirm'",
                    [RUN],
                    |row| row.get(0),
                )
                .expect("confirm gate")
        };
        assert!(
            question.contains("SUP-2") && question.contains("10 laptops"),
            "{question}"
        );
        decide_gate(&fx, "confirm", serde_json::json!({"option": "approve"}));

        // (a) supplier_id comes from a GATE: the mutating create runs without asking.
        let created = execute_with(&fx, &registry, "run_step", run_step_args("create-pr")).await;
        let created: serde_json::Value = serde_json::from_str(&created).expect("json result");
        let args: serde_json::Value =
            serde_json::from_str(created["result"].as_str().expect("echo result")).expect("args");
        assert_eq!(
            args,
            serde_json::json!({
                "supplier_id": "SUP-2",
                "items": "10 laptops",
                "cost_centre": "CC-42",
                "need_by": "2026-12-01",
            })
        );
        let steps = fx.store.list_playbook_steps(RUN).expect("steps");
        let create = steps
            .iter()
            .find(|s| s.step_id == "create-pr")
            .expect("step");
        assert_eq!(create.outcome, "done");
        assert!(!create.evidence_refs.is_empty());
        let status = created["status"].as_str().expect("status");
        assert!(
            status.contains("pick-supplier.supplier_id = \"SUP-2\""),
            "{status}"
        );
    }

    fn chain(second_tool: &str, first_mode: StepMode) -> PlaybookSpec {
        let mut first = step(
            "s1",
            first_mode,
            if first_mode == StepMode::Bound {
                Some("looker")
            } else {
                None
            },
        );
        first.instruction = "look it up".to_string();
        let mut second = step("s2", StepMode::Bound, Some(second_tool));
        second.after = vec!["s1".to_string()];
        second.instruction = "act".to_string();
        second.binding = Some(vam_spec::v1::StepBinding {
            tool_name: second_tool.to_string(),
            arguments: json_to_struct(&serde_json::json!({"id": "{{steps.s1.output.id}}"})),
        });
        playbook(
            PB,
            "a chain",
            ActivationMode::Ask,
            false,
            vec![first, second],
        )
    }

    fn pending_count(fx: &Fx) -> i64 {
        let connection = fx.store.connection.lock().expect("lock");
        connection
            .query_row(
                "SELECT COUNT(*) FROM hitl_pending WHERE run_id = ?1",
                [RUN],
                |r| r.get(0),
            )
            .expect("count")
    }

    #[tokio::test]
    async fn a_mutating_step_with_an_argument_from_a_bound_tool_output_asks_with_the_resolved_arguments(
    ) {
        let fx = with_tools(
            fixture(vec![chain("changer", StepMode::Bound)], vec![]),
            &[("looker", "echo", 1), ("changer", "echo", 3)],
        );
        let registry = flight_registry();
        bind(&fx, PB, None);
        // The lookup is a literal echo: its output is {"id": ...}.
        let first = fx.store.get_playbook_spec(RUN).expect("spec");
        assert_eq!(first.steps.len(), 2);
        fx.store
            .complete_playbook_step(
                RUN,
                "s1",
                &crate::workflow::PlaybookStepCompletion {
                    outcome: "done".to_string(),
                    output_json: Some(r#"{"id":"X1"}"#.to_string()),
                    agent_summary: None,
                },
            )
            .expect("s1");
        // s1 is bound in this fixture; its output is host-captured text in a real run.
        let parked = intent(&fx, "run_step", run_step_args("s2"));
        assert_eq!(
            parked.disposition,
            Disposition::Suspended as i32,
            "{:?}",
            parked.error
        );
        let (request_id, requested, schema, message) = pending_row(&fx);
        let requested: serde_json::Value = serde_json::from_str(&requested).expect("json");
        assert_eq!(
            requested,
            serde_json::json!({"step_id": "s2", "arguments": {"id": "X1"}})
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&schema).expect("schema")["x-vam-run-step"]
                ["tool"],
            "changer"
        );
        assert!(
            message.contains("changer") && message.contains("step 's1'"),
            "{message}"
        );
        // Executing without the approval is refused; nothing ran.
        let plan =
            crate::playbook_flight::plan_bound_step(&fx.store, SESSION, RUN, "s2").expect("plan");
        assert!(plan.ask);
        let digest: String = {
            let connection = fx.store.connection.lock().expect("lock");
            connection
                .query_row(
                    "SELECT args_digest FROM hitl_pending WHERE request_id = ?1",
                    [&request_id],
                    |r| r.get(0),
                )
                .expect("digest")
        };
        assert_eq!(digest, plan.grant_digest);
        // An approval for exactly those resolved arguments lets the retry run.
        grant(&fx, &digest);
        let ran = execute_with(&fx, &registry, "run_step", run_step_args("s2")).await;
        assert!(ran.contains("X1"), "{ran}");
    }

    #[tokio::test]
    async fn a_mutating_step_with_an_argument_from_an_agentic_step_output_asks() {
        let fx = with_tools(
            fixture(vec![chain("changer", StepMode::Agentic)], vec![]),
            &[("changer", "echo", 3)],
        );
        bind(&fx, PB, None);
        fx.store
            .complete_playbook_step(
                RUN,
                "s1",
                &crate::workflow::PlaybookStepCompletion {
                    outcome: "done".to_string(),
                    output_json: Some(r#"{"id":"M7"}"#.to_string()),
                    agent_summary: None,
                },
            )
            .expect("s1");
        let parked = intent(&fx, "run_step", run_step_args("s2"));
        assert_eq!(parked.disposition, Disposition::Suspended as i32);
        let (_, requested, _, _) = pending_row(&fx);
        assert!(requested.contains("M7"), "{requested}");
        assert_eq!(pending_count(&fx), 1);
    }

    #[tokio::test]
    async fn a_read_only_tool_with_tool_sourced_arguments_runs_without_asking() {
        let fx = with_tools(
            fixture(vec![chain("reader", StepMode::Bound)], vec![]),
            &[("looker", "echo", 1), ("reader", "echo", 1)],
        );
        let registry = flight_registry();
        bind(&fx, PB, None);
        fx.store
            .complete_playbook_step(
                RUN,
                "s1",
                &crate::workflow::PlaybookStepCompletion {
                    outcome: "done".to_string(),
                    output_json: Some(r#"{"id":"R1"}"#.to_string()),
                    agent_summary: None,
                },
            )
            .expect("s1");
        let ran = execute_with(&fx, &registry, "run_step", run_step_args("s2")).await;
        assert!(ran.contains("R1"), "{ran}");
        assert_eq!(pending_count(&fx), 0);
    }

    #[tokio::test]
    async fn start_inputs_resolve_in_bound_arguments_and_count_as_user_supplied() {
        let mut pb = playbook(
            PB,
            "an input playbook",
            ActivationMode::Ask,
            true,
            vec![step("only", StepMode::Bound, Some("changer"))],
        );
        pb.steps[0].binding = Some(vam_spec::v1::StepBinding {
            tool_name: "changer".to_string(),
            arguments: json_to_struct(&serde_json::json!({"title": "{{inputs.title}}"})),
        });
        let fx = with_tools(fixture(vec![pb], vec![]), &[("changer", "echo", 3)]);
        let registry = flight_registry();
        bind(&fx, PB, Some(serde_json::json!({"title": "Fix the bug"})));
        let ran = execute_with(&fx, &registry, "run_step", run_step_args("only")).await;
        assert!(ran.contains("Fix the bug"), "{ran}");
        assert_eq!(pending_count(&fx), 0);
    }

    #[tokio::test]
    async fn a_guidance_only_playbook_adds_its_text_and_tracks_nothing() {
        let pb = PlaybookSpec {
            resource_id: Some(ResourceId {
                urn: PB.to_string(),
            }),
            display_name: "a style guide".to_string(),
            guidance: "Write in the second person.".to_string(),
            activation: Some(PlaybookActivation {
                when: "Use when the user wants release notes.".to_string(),
                examples: vec![],
                mode: ActivationMode::Auto as i32,
            }),
            ..Default::default()
        };
        let fx = fixture(vec![pb], vec![]);
        let text = execute(&fx, START_PLAYBOOK_TOOL, start_args(PB, None)).await;
        assert!(
            text.starts_with(vam_spec::playbook::PLAYBOOK_GUIDANCE_PREFIX),
            "{text}"
        );
        assert!(text.contains("Write in the second person."));
        assert!(!text.starts_with(vam_spec::playbook::PLAYBOOK_STARTED_PREFIX));
        assert!(fx.store.get_playbook_run(RUN).expect("row").is_none());
        assert!(crate::playbook_flight::status_block(&fx.store, RUN).is_none());
    }
}
