//! The one way an approval is decided (NT-2.4; docs: orchestrator/notifications).
//!
//! [`decide_as`] is the body of the HTTP route `POST /v1/approvals/:id/decide`, moved here: the
//! "remember" rule write, the decision, the run state flip and the resume dispatch. It has two
//! callers and no copy:
//!
//! - the HTTP route, after it parsed the body, ran the policy check and the idempotency key. The
//!   surface is the client's `X-Vam-Surface` hint through `run_facts::surface_from_header`, which
//!   maps anything but `cli` and `app` to `api`: a client can never say `slack`. It passes
//!   `allow_remember: true`.
//! - the notification gateway (`notify_gateway.rs::act_on`), after its checklist. The user is the
//!   handle's user and the surface is the one stored on the handle `vamd` issued from the
//!   operator-approved manifest. It passes `allow_remember: false`.
//!
//! The approver check (`HitlApprover::allows_decider`), the gate and question validation and the
//! stored provenance (`decided_by_user_id`, `decided_surface`) are the store's
//! (`decide_approval_with_surface`) and unchanged.

use vam_security::ScopeRule;
use vam_spec::v1::ResourceId;

use super::{
    resume_run_after_approval, resume_run_after_gate, resume_run_after_rejection, uuid_like,
    AppState,
};
use crate::error::{ApprovalRecord, OrchestratorError, StoredApproval};

/// Who decides, and from where. The caller builds it from what it authenticated; nothing in a
/// request body or a broker's claim reaches it.
#[derive(Debug, Clone)]
pub struct DeciderCaller {
    /// The deciding user. The store's approver check and the provenance use it.
    pub user_id: String,
    /// The principal recorded as the reviewer.
    pub principal: String,
    /// `cli`, `app`, `api` from the route; the handle's surface from the gateway.
    pub surface: String,
    /// Whether an "always allow" or "always deny" rule may be written. Only the route may.
    pub allow_remember: bool,
}

/// What is decided, already parsed.
#[derive(Debug, Clone)]
pub struct DecideInput {
    /// `APPROVED`, `REJECTED`, `AMENDED` or `SUBMITTED`.
    pub decision: &'static str,
    /// A standing rule to write with the decision (refused unless `allow_remember`).
    pub remember: Option<ScopeRule>,
    pub rejection_reason: String,
    pub amended_args_json: Option<String>,
    pub response_json: Option<String>,
    pub expected_revision: Option<i64>,
}

/// What a decision came to.
#[derive(Debug, Clone)]
pub struct Decided {
    pub record: ApprovalRecord,
    /// The approval row right after the decision, before the run resumes.
    pub stored: StoredApproval,
}

/// Decides `request_id` as `caller`. See the module docs.
pub async fn decide_as(
    state: &AppState,
    caller: DeciderCaller,
    request_id: ResourceId,
    input: DecideInput,
) -> Result<Decided, OrchestratorError> {
    if input.remember.is_some() && !caller.allow_remember {
        return Err(OrchestratorError::PermissionDenied(
            "a standing rule is set in the app or the CLI, not from here".into(),
        ));
    }
    let decision = input.decision;
    let token_id = ResourceId {
        urn: format!("urn:vam:token:local.vam.dev/acme/{}", uuid_like()),
    };
    // Write the agent rule first, then record the decision. A retry rewrites
    // the identical rule (no new revision) and then decides, so a partial
    // failure leaves neither a duplicate rule nor a double decision. Nothing is
    // saved unless the decision itself can still be recorded.
    let rejection_reason = match (input.remember, input.rejection_reason.is_empty()) {
        (Some(ScopeRule::Deny), true) => "Always deny".to_string(),
        _ => input.rejection_reason.clone(),
    };
    if let Some(rule) = input.remember {
        let pending = state
            .store
            .get_approval_for_owner(&request_id.urn, &caller.user_id)?;
        let can_decide = pending.published
            && pending.decision.is_none()
            && input
                .expected_revision
                .is_none_or(|expected| expected == pending.decision_revision);
        if can_decide {
            if !crate::hitl::HitlApprover::parse(&pending.approver_user_id)
                .allows_decider(&caller.user_id)
            {
                return Err(OrchestratorError::PermissionDenied(
                    "caller is not the selected HITL approver".into(),
                ));
            }
            let plan = state.store.plan_remember(&pending, &caller.user_id, rule)?;
            state.store.save_remembered_rule(&plan)?;
        }
    }
    let record = state.store.decide_approval_with_surface(
        &request_id,
        &token_id,
        decision,
        &caller.principal,
        &caller.user_id,
        if rejection_reason.is_empty() {
            None
        } else {
            Some(rejection_reason.as_str())
        },
        input.amended_args_json.as_deref(),
        input.expected_revision,
        input.response_json.as_deref(),
        &caller.surface,
    )?;

    let stored = state.store.get_approval(&request_id.urn)?;
    let stored_kind = stored.kind.clone();
    let is_gate = stored_kind == crate::hitl::APPROVAL_KIND_GATE;
    // Which decisions resume a parked run: a gate's, a tool call's and a question's answer.
    let resumes = is_gate
        || matches!(decision, "APPROVED" | "AMENDED" | "REJECTED")
        || (stored_kind == crate::hitl::APPROVAL_KIND_QUESTION && decision == "SUBMITTED");
    // One resume per parked run. The decision that moves the run PAUSED -> RUNNING owns it. A
    // decision that finds the run already resumed (or not waiting) is recorded and starts
    // nothing: the resume in flight reads it when it builds its inputs, or tells the task about
    // it at its next resume (`resume_decisions`).
    let owns_resume = resumes && state.store.claim_parked_run(&record.run_id)?;
    if resumes && !owns_resume {
        eprintln!(
            "[orchestrator] decision {} on run {} recorded without a second resume: the run is not parked (a resume of it is in flight)",
            record.request_id, record.run_id
        );
    }

    let manager = state.manager.clone();
    let store_bg = state.store.clone();
    let record_bg = record.clone();
    let decision_bg = decision.to_string();
    let kind_bg = stored_kind;
    tokio::spawn(async move {
        if !owns_resume {
            return;
        }
        if kind_bg == crate::hitl::APPROVAL_KIND_GATE {
            if let Err(e) =
                resume_run_after_gate(manager.clone(), store_bg.clone(), record_bg.clone()).await
            {
                eprintln!("[orchestrator] resume after gate decision failed: {}", e);
                let result_json = serde_json::json!({ "error": e.to_string() }).to_string();
                let _ =
                    store_bg.complete_run_with_result(&record_bg.run_id, "FAILED", &result_json);
                let _ = manager
                    .terminate_session(record_bg.session_id.clone())
                    .await;
            }
            return;
        }
        if kind_bg == crate::hitl::APPROVAL_KIND_QUESTION && decision_bg == "SUBMITTED" {
            if let Err(e) = super::question::resume_run_after_answer(
                manager.clone(),
                store_bg.clone(),
                record_bg.clone(),
            )
            .await
            {
                eprintln!("[orchestrator] resume after answer failed: {}", e);
                let result_json = serde_json::json!({ "error": e.to_string() }).to_string();
                let _ =
                    store_bg.complete_run_with_result(&record_bg.run_id, "FAILED", &result_json);
                let _ = manager
                    .terminate_session(record_bg.session_id.clone())
                    .await;
            }
        } else if matches!(decision_bg.as_str(), "APPROVED" | "AMENDED") {
            if let Err(e) =
                resume_run_after_approval(manager.clone(), store_bg.clone(), record_bg.clone())
                    .await
            {
                eprintln!("[orchestrator] resume after approval failed: {}", e);
                let result_json = serde_json::json!({ "error": e.to_string() }).to_string();
                let _ =
                    store_bg.complete_run_with_result(&record_bg.run_id, "FAILED", &result_json);
                // Crash/early-error paths may not have reached finalize_run_session;
                // always release the kept-alive HITL session so the workspace is not pinned.
                let _ = manager
                    .terminate_session(record_bg.session_id.clone())
                    .await;
            }
        } else if decision_bg == "REJECTED" {
            if let Err(e) =
                resume_run_after_rejection(manager.clone(), store_bg.clone(), record_bg.clone())
                    .await
            {
                eprintln!("[orchestrator] resume after rejection failed: {}", e);
                let result_json = serde_json::json!({ "error": e.to_string() }).to_string();
                let _ =
                    store_bg.complete_run_with_result(&record_bg.run_id, "FAILED", &result_json);
                let _ = manager
                    .terminate_session(record_bg.session_id.clone())
                    .await;
            }
        }
    });

    Ok(Decided { record, stored })
}
