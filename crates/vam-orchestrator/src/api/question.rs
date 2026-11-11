//! Questions (AR-3.10): what a parked run waits for, and the resume after an
//! answer. The checks on the answer itself live in the store's decide
//! (`hitl::validate_question_answer`), so a bad answer never reaches this file.

use std::sync::Arc;

use vam_spec::v1::run_status::RunState;
use vam_spec::v1::WaitingFor;

use super::{
    build_hitl_resume_inputs, resume_run_after_hitl_decision, struct_from_json_value,
    OrchestratorError, SqliteWorkflowStore,
};
use crate::manager::SessionManager;

/// What a run is waiting for, set only while it is PAUSED.
pub(super) fn waiting_for(store: &SqliteWorkflowStore, run_id: &str, state: i32) -> i32 {
    if state != RunState::Paused as i32 {
        return WaitingFor::Unspecified as i32;
    }
    match store.pending_approval_kind(run_id).as_deref() {
        Some(crate::hitl::APPROVAL_KIND_QUESTION) => WaitingFor::Input as i32,
        Some(crate::hitl::APPROVAL_KIND_GATE) => WaitingFor::Gate as i32,
        Some(crate::hitl::APPROVAL_KIND_REAUTH) => WaitingFor::Reauth as i32,
        Some(_) => WaitingFor::Approval as i32,
        None => WaitingFor::Unspecified as i32,
    }
}

/// Resume a run after its question was answered. The answer, already checked,
/// goes to the task as `ask_user`'s tool result (`hitl_answer`).
pub(super) async fn resume_run_after_answer(
    manager: SessionManager,
    store: Arc<SqliteWorkflowStore>,
    record: crate::error::ApprovalRecord,
) -> Result<(), OrchestratorError> {
    let stored = store.get_approval(&record.request_id)?;
    let inputs = answer_resume_inputs(store.as_ref(), &stored, &record)?;
    resume_run_after_hitl_decision(manager, store, record, inputs).await
}

/// The inputs of the resume after a question was answered.
pub(super) fn answer_resume_inputs(
    store: &SqliteWorkflowStore,
    stored: &crate::error::StoredApproval,
    record: &crate::error::ApprovalRecord,
) -> Result<prost_types::Struct, OrchestratorError> {
    let answer: serde_json::Value = stored
        .amended_args_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .ok_or_else(|| OrchestratorError::CorruptState("the stored answer is missing".into()))?;
    let answer = struct_from_json_value(&answer).ok_or_else(|| {
        OrchestratorError::CorruptState("the stored answer is not an object".into())
    })?;
    let mut inputs = build_hitl_resume_inputs(
        store,
        &record.run_id,
        &record.tool_name,
        &stored.requested_args_json,
    )?;
    inputs.fields.insert(
        "hitl_answer".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StructValue(answer)),
        },
    );
    Ok(inputs)
}
