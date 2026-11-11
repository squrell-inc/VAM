//! What the one resume of a parked run is told (`crate::resume_decisions`).
//!
//! The decision that owns the resume built the task's inputs from itself alone. Before the task
//! starts, this reads every decision recorded for the run (a gate decided together with a
//! question, a tool call decided while a gate was open, a late decision of an earlier resume) and
//! makes sure the task sees them all:
//!
//! - The task resumes from the decision that has a call waiting for its result (a question's
//!   answer, an approved or denied tool call), whichever decision owns the resume. A gate leaves
//!   no call waiting, so it only resumes the task when it is the sole decision.
//! - The other decisions are written into the task's resume note, one sentence each.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::SqliteWorkflowStore;
use crate::error::{ApprovalRecord, StoredApproval};

/// How long the owner of a resume waits for a decision that is still being made on another of
/// the run's waits, before it reads the decisions recorded so far. A person (or the app) that
/// decides a gate and a question together sends both within moments of each other.
const SIBLING_SETTLE: Duration = Duration::from_millis(400);
const SIBLING_POLL: Duration = Duration::from_millis(20);

/// Marks the decisions the resume was not told about as late when the resumed task ends or parks
/// again, and says so in the run's events.
pub(super) struct LateDecisions {
    store: Arc<SqliteWorkflowStore>,
    run_id: String,
}

impl LateDecisions {
    pub(super) fn new(store: Arc<SqliteWorkflowStore>, run_id: &str) -> Self {
        Self {
            store,
            run_id: run_id.to_string(),
        }
    }
}

impl Drop for LateDecisions {
    fn drop(&mut self) {
        let late = match self.store.mark_decisions_late(&self.run_id) {
            Ok(late) => late,
            Err(e) => {
                tracing::warn!(run = %self.run_id, "could not mark late decisions: {e}");
                return;
            }
        };
        for request_id in late {
            let stored = self.store.get_approval(&request_id).ok();
            eprintln!(
                "[orchestrator] decision {request_id} on run {} landed after the resume read its decisions; it starts nothing and the next resume reports it",
                self.run_id
            );
            crate::trajectory::append_trajectory_event(
                &self.store,
                &self.run_id,
                crate::events::TYPE_DECISION_DEFERRED,
                &serde_json::json!({
                    "request_id": request_id,
                    "kind": stored.as_ref().map(|s| s.kind.clone()),
                    "decision": stored.as_ref().and_then(|s| s.decision.clone()),
                    "outcome": "recorded; not part of the resume that was running; reported at the next resume",
                }),
            );
        }
    }
}

/// The inputs of the one resume: the owner's own, or those of the decision that has a call
/// waiting, with every other recorded decision written into the resume note.
pub(crate) async fn compose_resume_inputs(
    store: &Arc<SqliteWorkflowStore>,
    record: &ApprovalRecord,
    own_inputs: prost_types::Struct,
) -> prost_types::Struct {
    // Another wait of the run may be decided in the next moment: give it the chance to be read.
    let deadline = Instant::now() + SIBLING_SETTLE;
    while store.undecided_approval_count(&record.run_id).unwrap_or(0) > 0
        && Instant::now() < deadline
    {
        tokio::time::sleep(SIBLING_POLL).await;
    }
    let taken = match store.take_recorded_decisions(&record.run_id) {
        Ok(taken) => taken,
        Err(e) => {
            tracing::warn!(run = %record.run_id, "could not read the recorded decisions: {e}");
            return own_inputs;
        }
    };
    let Some(own) = taken
        .fresh
        .iter()
        .find(|s| s.request_id == record.request_id)
    else {
        // The owner's decision was already told to a resume (a direct call, a retry): nothing to add.
        return own_inputs;
    };
    // The decision with a call waiting for its result resumes the task; a gate does not.
    let is_gate = |s: &StoredApproval| s.kind == crate::hitl::APPROVAL_KIND_GATE;
    let primary = if is_gate(own) {
        taken.fresh.iter().find(|s| !is_gate(s)).unwrap_or(own)
    } else {
        own
    };
    let others: Vec<&StoredApproval> = taken
        .fresh
        .iter()
        .filter(|s| s.request_id != primary.request_id)
        .collect();
    if others.is_empty() && taken.late.is_empty() {
        return own_inputs;
    }

    let mut inputs = if primary.request_id == own.request_id && !others.iter().any(|s| is_gate(s)) {
        own_inputs
    } else {
        // Rebuilt after the reads: its playbook status already shows every decided gate.
        match super::resume_inputs_for(store.as_ref(), primary, &record_of(primary)) {
            Ok(inputs) => inputs,
            Err(e) => {
                tracing::warn!(run = %record.run_id, "could not rebuild the resume inputs: {e}");
                return own_inputs;
            }
        }
    };

    let mut lines: Vec<String> = Vec::new();
    for stored in &others {
        lines.push(describe(store, stored));
    }
    let mut note = String::new();
    if !lines.is_empty() {
        note.push_str("Other decisions were recorded while the run was parked:\n");
        note.push_str(&lines.join("\n"));
    }
    if !taken.late.is_empty() {
        if !note.is_empty() {
            note.push('\n');
        }
        note.push_str(
            "Decisions that landed after the run last resumed (they changed nothing then):\n",
        );
        let late: Vec<String> = taken.late.iter().map(|s| describe(store, s)).collect();
        note.push_str(&late.join("\n"));
    }
    if others.iter().any(|s| is_gate(s)) && !is_gate(primary) {
        if let Some(status) = crate::playbook_flight::status_block(store.as_ref(), &record.run_id) {
            note.push_str("\n\n");
            note.push_str(vam_spec::playbook::PLAYBOOK_GATE_RULE);
            note.push_str("\n\n");
            note.push_str(&status);
        }
    }
    attach_note(&mut inputs, primary, &note);

    crate::trajectory::append_trajectory_event(
        store,
        &record.run_id,
        crate::events::TYPE_RESUME_DECISIONS,
        &serde_json::json!({
            "resumed_by": primary.request_id,
            "delivered": taken.fresh.iter().map(summary).collect::<Vec<_>>(),
            "late": taken.late.iter().map(summary).collect::<Vec<_>>(),
        }),
    );
    inputs
}

fn summary(s: &StoredApproval) -> serde_json::Value {
    serde_json::json!({
        "request_id": s.request_id,
        "kind": s.kind,
        "decision": s.decision,
    })
}

fn record_of(s: &StoredApproval) -> ApprovalRecord {
    ApprovalRecord {
        request_id: s.request_id.clone(),
        run_id: s.run_id.clone(),
        step_id: s.step_id.clone(),
        attempt_id: s.attempt_id.clone(),
        session_id: s.session_id.clone(),
        checkpoint_digest: s.checkpoint_digest.clone(),
        continuation_generation: s.continuation_generation,
        tool_name: s.tool_name.clone(),
        args_digest: s.args_digest.clone(),
        decision_revision: s.decision_revision,
    }
}

/// One sentence for what the user decided.
fn describe(store: &SqliteWorkflowStore, s: &StoredApproval) -> String {
    let decision = s.decision.as_deref().unwrap_or_default();
    if s.kind == crate::hitl::APPROVAL_KIND_GATE {
        let outcome = store
            .list_playbook_steps(&s.run_id)
            .ok()
            .and_then(|steps| {
                steps
                    .into_iter()
                    .find(|step| step.step_id == s.step_id)
                    .map(|step| step.outcome)
            })
            .filter(|o| !o.is_empty())
            .unwrap_or_else(|| decision.to_lowercase());
        return format!("Gate '{}' was decided with outcome '{outcome}'.", s.step_id);
    }
    if s.kind == crate::hitl::APPROVAL_KIND_QUESTION {
        let question = if s.approval_message.is_empty() {
            "your question".to_string()
        } else {
            format!("your question \"{}\"", s.approval_message)
        };
        return match decision {
            "SUBMITTED" => format!(
                "The user answered {question}: {}",
                s.amended_args_json.as_deref().unwrap_or("{}")
            ),
            _ => format!("The user turned down {question}."),
        };
    }
    match decision {
        "APPROVED" | "AMENDED" => format!(
            "The request to run {} was approved after the run had moved on, so it did not run. Call it again if the work still needs it.",
            s.tool_name
        ),
        _ => format!(
            "The request to run {} was denied: {}",
            s.tool_name,
            s.rejection_reason.as_deref().unwrap_or("denied by the user")
        ),
    }
}

/// Puts the note where the guest reads it for this kind of resume: after the answer of a
/// question, or at the end of the resume note it already has.
fn attach_note(inputs: &mut prost_types::Struct, primary: &StoredApproval, note: &str) {
    use prost_types::value::Kind;
    let key = if primary.kind == crate::hitl::APPROVAL_KIND_QUESTION
        && primary.decision.as_deref() == Some("SUBMITTED")
    {
        "hitl_decisions_note"
    } else if primary.decision.as_deref() == Some("REJECTED")
        && primary.kind != crate::hitl::APPROVAL_KIND_GATE
    {
        "hitl_denied_note"
    } else {
        "hitl_resume_note"
    };
    let merged = match inputs.fields.get(key).and_then(|v| v.kind.as_ref()) {
        Some(Kind::StringValue(existing)) if !existing.is_empty() => {
            format!("{existing}\n\n{note}")
        }
        _ => note.to_string(),
    };
    inputs.fields.insert(
        key.to_string(),
        prost_types::Value {
            kind: Some(Kind::StringValue(merged)),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use vam_spec::v1::ResourceId;

    const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-late-decision";

    /// A decision that lands after the resume read its decisions starts nothing; when the
    /// resumed task ends, the decision is marked late, the run's events say so, and the next
    /// resume tells the task about it.
    #[tokio::test]
    async fn a_decision_after_the_read_is_reported_in_the_events_and_at_the_next_resume() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
        store
            .create_run(&ResourceId {
                urn: RUN.to_string(),
            })
            .expect("run");
        let guard = LateDecisions::new(store.clone(), RUN);
        {
            // The answer is recorded while the resumed task runs.
            let connection = store.connection.lock().expect("connection");
            connection
                .execute(
                    "INSERT INTO approvals (
                        request_id, run_id, step_id, attempt_id, checkpoint_digest,
                        continuation_generation, decision, published, kind, tool_name,
                        approval_message, amended_args_json, owner_user_id, approver_user_id,
                        decided_at, created_at
                     ) VALUES ('q-late', ?1, 'step', 'attempt', 'digest', 1, 'SUBMITTED', 1,
                        'question', 'ask_user', 'Which title?', '{\"answer\":\"Q4 plan\"}',
                        'local', 'local', 100, 100)",
                    params![RUN],
                )
                .expect("insert");
        }
        drop(guard);

        let events = store.read_outbox_for_run(RUN, 0).expect("outbox");
        let deferred: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == crate::events::TYPE_DECISION_DEFERRED)
            .collect();
        assert_eq!(
            deferred.len(),
            1,
            "one event says the decision was left out"
        );
        assert!(deferred[0].payload.contains("q-late"));

        // The next resume of the run is told, in words, and the decision counts as told.
        let taken = store.take_recorded_decisions(RUN).expect("take");
        assert_eq!(taken.late.len(), 1);
        let line = describe(&store, &taken.late[0]);
        assert!(
            line.contains("Which title?") && line.contains("Q4 plan"),
            "{line}"
        );
    }
}
