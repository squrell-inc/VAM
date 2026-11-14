//! Host side of the run inbox (AR-3.5, AR-3.6, design V.3).
//!
//! The API (`api/inbox.rs`) puts messages in `run_inbox`; the guest takes them
//! in at turn boundaries through the port-52 method `PullInbox`. A message
//! stays `pending` until an acknowledgement names it: the next `PullInbox`
//! carries the last seq the guest applied, and so does a suspend tail. Then the
//! row is `delivered`, its text is cleared (the thread log holds the
//! conversation, locked decision 23) and `inbox.delivered` is emitted.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use vam_spec::v1::{ErrorStatus, InboxMessage, PullInboxRequest, PullInboxResponse, RunSpec};

use crate::error::OrchestratorError;
use crate::events::{
    TYPE_INBOX_DELIVERED, TYPE_INBOX_DISCARDED, TYPE_INBOX_RECEIVED, TYPE_INBOX_STARTED,
};
use crate::inbox_store::{InboxRow, NewInboxRow, NewQueuedRun};
use crate::run_checkpoint::bind_checkpoint_lease;
use crate::workflow::SqliteWorkflowStore;

/// Most message text in one `PullInbox` reply. A port-52 frame is small; the
/// guest asks again until an empty batch. At least one message is always sent.
pub const PULL_REPLY_BUDGET: usize = 48 * 1024;

fn refusal(code: i32, message: impl Into<String>) -> PullInboxResponse {
    PullInboxResponse {
        messages: vec![],
        error: Some(ErrorStatus {
            code,
            message: message.into(),
            details: vec![],
            retryable: false,
        }),
    }
}

/// Answer one `PullInbox`: acknowledge everything the guest says it has applied
/// (`ack_seq`; `after_seq` is only the fetch cursor), then return the pending steers
/// after the cursor, a bounded batch in seq order.
pub fn handle_pull_inbox(
    store: &Arc<SqliteWorkflowStore>,
    session_id: &str,
    request: &PullInboxRequest,
) -> PullInboxResponse {
    let bound = match bind_checkpoint_lease(store, session_id, request.lease.as_ref()) {
        Ok(bound) => bound,
        Err((code, message)) => return refusal(code, message),
    };
    if request.after_seq < 0 || request.ack_seq < 0 {
        return refusal(3, "after_seq and ack_seq must not be negative");
    }
    let round = store.thread_recorder(session_id).map(|r| r.rounds());
    ack_through(store, &bound.run_id, request.ack_seq, round);
    match store.pending_steers_after(&bound.run_id, request.after_seq, PULL_REPLY_BUDGET) {
        Ok(rows) => PullInboxResponse {
            messages: rows
                .into_iter()
                .map(|row| InboxMessage {
                    seq: row.seq,
                    text: row.text,
                })
                .collect(),
            error: None,
        },
        Err(err) => refusal(13, err.to_string()),
    }
}

/// Mark the run's steers up to `seq` delivered and say so. A failure is logged:
/// the next acknowledgement retries it.
pub fn ack_through(store: &SqliteWorkflowStore, run_id: &str, seq: i64, round: Option<u64>) {
    if seq <= 0 {
        return;
    }
    match store.mark_steers_delivered(run_id, seq) {
        Ok(seqs) if !seqs.is_empty() => {
            let mut payload = serde_json::json!({ "seqs": seqs });
            if let Some(round) = round {
                payload["round"] = serde_json::json!(round);
            }
            crate::trajectory::append_trajectory_event(
                store,
                run_id,
                TYPE_INBOX_DELIVERED,
                &payload,
            );
        }
        Ok(_) => {}
        Err(err) => tracing::warn!("inbox acknowledgement for {run_id} failed: {err}"),
    }
}

/// `inbox.received {seq, mode}`: never the text.
pub fn emit_received(store: &SqliteWorkflowStore, run_id: &str, seq: i64, mode: &str) {
    crate::trajectory::append_trajectory_event(
        store,
        run_id,
        TYPE_INBOX_RECEIVED,
        &serde_json::json!({ "seq": seq, "mode": mode }),
    );
}

/// Who sent a queued message, as the starter needs to start its run as them later. No
/// secret: the key's hash is never stored here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedCaller {
    pub key_id: String,
    pub name: String,
    pub principal: String,
    pub scopes: Vec<String>,
    pub owner_user_id: String,
    pub role: String,
    pub session_token: bool,
    /// Where the run came from; absent in rows queued before triggers (a person).
    #[serde(default)]
    pub source: Option<crate::api::run_start::RunSource>,
    /// The grant credential of a triggered fire.
    #[serde(default)]
    pub credential_id: Option<String>,
    /// Unix time after which the fire is no longer wanted.
    #[serde(default)]
    pub not_after: Option<i64>,
}

impl QueuedCaller {
    pub fn from_caller(caller: &crate::api::run_start::RunCaller) -> Self {
        Self {
            key_id: caller.key_id.clone(),
            name: caller.name.clone(),
            principal: caller.principal.clone(),
            scopes: caller.scopes.clone(),
            owner_user_id: caller.key_owner_user_id.clone(),
            role: caller.role.as_str().to_string(),
            session_token: caller.session_token,
            source: Some(caller.source.clone()),
            credential_id: caller.credential_id.clone(),
            not_after: caller.not_after,
        }
    }

    /// The caller again, for the start: the role is read from the user now (a user who
    /// was deactivated since sends nothing), and a key that was revoked since starts
    /// nothing. A caller stored without scopes (a merged steer) gets its role's defaults.
    pub fn to_caller(
        &self,
        store: &SqliteWorkflowStore,
    ) -> Result<crate::api::run_start::RunCaller, OrchestratorError> {
        use crate::users::UserRole;
        let owner = crate::owner::resolved_owner(&self.owner_user_id).to_string();
        let role = match store.get_user(&owner) {
            Ok(user) => {
                crate::users::reject_deactivated_user(&user)?;
                user.role
            }
            Err(OrchestratorError::NotFound(_)) => UserRole::from_db(&self.role),
            Err(e) => return Err(e),
        };
        if !self.session_token && !self.key_id.is_empty() {
            if let Ok(key) = store.get_api_key(&self.key_id) {
                if key.revoked_at.is_some() {
                    return Err(OrchestratorError::PermissionDenied(
                        "API key has been revoked".to_string(),
                    ));
                }
            }
        }
        let scopes = if self.scopes.is_empty() {
            crate::keys::default_scopes_for_role(role)
        } else {
            self.scopes.clone()
        };
        let run_owner = if crate::owner::is_bootstrap_key(&self.key_id) {
            crate::users::OPERATOR_USER_ID.to_string()
        } else if !self.owner_user_id.is_empty() {
            self.owner_user_id.clone()
        } else {
            crate::users::LOCAL_USER_ID.to_string()
        };
        let users_admin = scopes.iter().any(|s| s == crate::keys::USERS_ADMIN_SCOPE);
        Ok(crate::api::run_start::RunCaller {
            owner_user_id: run_owner,
            principal: self.principal.clone(),
            role,
            scopes,
            credential_id: self.credential_id.clone(),
            users_admin,
            source: self
                .source
                .clone()
                .unwrap_or(crate::api::run_start::RunSource::Person {
                    surface: crate::run_facts::SURFACE_API.to_string(),
                }),
            not_after: self.not_after,
            key_id: self.key_id.clone(),
            name: self.name.clone(),
            session_token: self.session_token,
            key_owner_user_id: self.owner_user_id.clone(),
        })
    }
}

/// What `run_inbox.queued_spec_json` holds for a queued message: the request it will start
/// (as the client sent it, with the workspace as a plain name) and who sent it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedStart {
    /// `RunSpec` JSON.
    pub request: String,
    pub caller: QueuedCaller,
}

/// The thread's message `text` joined the way the guest joins steers (Q4).
pub fn join_steers<'a>(texts: impl IntoIterator<Item = &'a str>) -> String {
    texts.into_iter().collect::<Vec<_>>().join("\n\n")
}

/// The request a queued run starts from, given its resolved spec: it keeps the Space and
/// thread ids and the run starts fresh.
/// `prompt_only` keeps just the prompt and the model: a steer merged at a task's end carries
/// text only.
pub fn request_from_resolved(resolved: &RunSpec, prompt_only: bool) -> RunSpec {
    // The resolved spec already holds `volume://sp_…`, `space_id` and `thread_id`: the queued
    // request carries ids, so it cannot land in another place.
    let mut spec = resolved.clone();
    spec.run_id = None;
    spec.inbox_mode = 0;
    if prompt_only {
        spec.playbook_ref = None;
        if let Some(inputs) = spec.inputs.as_mut() {
            inputs
                .fields
                .retain(|k, _| matches!(k.as_str(), "prompt" | "model" | "model_provider"));
        }
    }
    spec
}

/// `inbox.discarded {seqs}` for what a stop (or a cancel) threw away; returns the messages
/// with their text, for the answer.
pub fn discard_for_stop(
    store: &SqliteWorkflowStore,
    run_id: &str,
) -> Result<Vec<InboxRow>, OrchestratorError> {
    let rows = store.discard_run_inbox(run_id)?;
    if !rows.is_empty() {
        crate::trajectory::append_trajectory_event(
            store,
            run_id,
            TYPE_INBOX_DISCARDED,
            &serde_json::json!({ "seqs": rows.iter().map(|r| r.seq).collect::<Vec<_>>() }),
        );
    }
    Ok(rows)
}

/// `inbox.started {seq, runId}` on the run the message was queued behind.
pub fn emit_started(store: &SqliteWorkflowStore, run_id: &str, seq: i64, started_run_id: &str) {
    crate::trajectory::append_trajectory_event(
        store,
        run_id,
        TYPE_INBOX_STARTED,
        &serde_json::json!({ "seq": seq, "runId": started_run_id }),
    );
}

/// Which of an ended run's undelivered steers the thread log already holds. A guest that
/// died after applying a steer, before it acknowledged, leaves the row pending while the
/// conversation has the text; running it again would do the work twice.
///
/// The guest delivers all pending steers of a boundary as one user message, texts joined by
/// a blank line, so a steer is in the log when a user message of the run equals it, or equals
/// it joined with the steers that follow. The task's own prompt (the run's first user
/// message) is never a match. Each logged message accounts for one group only.
pub fn steers_in_log(user_messages: &[String], steers: &[(i64, String)]) -> Vec<i64> {
    let mut available: Vec<&str> = user_messages.iter().map(String::as_str).collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i < steers.len() {
        let mut matched = None;
        // Longest group first: the whole batch the guest joined.
        for j in (i..steers.len()).rev() {
            let joined = join_steers(steers[i..=j].iter().map(|(_, t)| t.as_str()));
            if let Some(at) = available.iter().position(|m| *m == joined) {
                matched = Some((j, at));
                break;
            }
        }
        match matched {
            Some((j, at)) => {
                available.remove(at);
                found.extend(steers[i..=j].iter().map(|(seq, _)| *seq));
                i = j + 1;
            }
            None => i += 1,
        }
    }
    found
}

/// The user messages the thread log holds for `run_id`, after the task's own prompt.
pub fn logged_user_messages(
    store: &SqliteWorkflowStore,
    run_id: &str,
) -> Result<Vec<String>, OrchestratorError> {
    let (spec_json, _, _) = store.get_run_spec_context(run_id)?;
    let spec: RunSpec = spec_json
        .and_then(|json| vam_spec::encoding::json_to_proto("vam.v1.RunSpec", &json).ok())
        .ok_or_else(|| OrchestratorError::NotFound(format!("run '{run_id}' has no spec")))?;
    let workspace_id = crate::run_context::parse_workspace_ref_uri(&spec.workspace_ref_uri)?;
    let thread = crate::run_context::extract_thread_ref(&spec)?;
    Ok(logged_user_messages_in(
        &crate::manager::drive9_layout(),
        &workspace_id,
        &thread,
        run_id,
    ))
}

/// [`logged_user_messages`] for a workspace and thread, read from the committed HEAD.
pub fn logged_user_messages_in(
    layout: &vam_storage::Drive9Layout,
    workspace_id: &str,
    thread: &str,
    run_id: &str,
) -> Vec<String> {
    use vam_storage::thread_log::{HeadThreadReader, ThreadLogReader};
    let chunks = vam_storage::ChunkStore::new(layout.clone());
    let manifests = vam_storage::ManifestManager::new(layout.clone(), chunks.clone());
    let reader = HeadThreadReader::new(&manifests, &chunks, workspace_id);
    let Ok(page) = reader.read(thread, 0, usize::MAX) else {
        // No log (a thread that never had one): nothing is in it.
        return Vec::new();
    };
    let mut messages: Vec<String> = page
        .records
        .iter()
        .filter(|r| r.run == run_id && r.lane == vam_storage::thread_log::MAIN_LANE)
        .filter_map(|r| r.message())
        .filter(|m| m.role == "user")
        .map(|m| m.to_wire().content)
        .collect();
    if !messages.is_empty() {
        messages.remove(0);
    }
    messages
}

/// The queued run an ended run's undelivered steers become, built from the ended run's own
/// spec with the merged text as the prompt.
pub fn merged_task(
    store: &SqliteWorkflowStore,
    run_id: &str,
    steers: &[InboxRow],
) -> Result<(NewInboxRow, NewQueuedRun), OrchestratorError> {
    let (spec_json, _, agent_urn) = store.get_run_spec_context(run_id)?;
    let resolved: RunSpec = spec_json
        .and_then(|json| vam_spec::encoding::json_to_proto("vam.v1.RunSpec", &json).ok())
        .ok_or_else(|| OrchestratorError::NotFound(format!("run '{run_id}' has no spec")))?;
    let first = steers
        .first()
        .ok_or_else(|| OrchestratorError::Core("no steers".into()))?;
    let text = join_steers(steers.iter().map(|r| r.text.as_str()));
    let key = format!("steer-merge-{run_id}-{}", first.seq);
    let new_id = format!(
        "urn:vam:run:local.vam.dev/acme/run-{}",
        crate::api::uuid_like()
    );
    let mut request = request_from_resolved(&resolved, true);
    request.idempotency_key = key.clone();
    set_prompt(&mut request, &text);
    let mut display = resolved.clone();
    display.run_id = Some(vam_spec::v1::ResourceId {
        urn: new_id.clone(),
    });
    display.inbox_mode = 0;
    display.playbook_ref = None;
    set_prompt(&mut display, &text);
    let (principal, owner) = store.run_principal_and_owner(run_id)?;
    let caller = QueuedCaller {
        key_id: String::new(),
        name: "steer merged at task end".into(),
        principal: principal.clone(),
        scopes: Vec::new(),
        owner_user_id: owner.clone(),
        role: String::new(),
        session_token: false,
        source: None,
        credential_id: None,
        not_after: None,
    };
    let start = QueuedStart {
        request: vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &request)
            .map_err(OrchestratorError::Core)?,
        caller,
    };
    let queued = NewQueuedRun {
        run_id: new_id,
        spec_json: vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &display)
            .map_err(OrchestratorError::Core)?,
        agent_urn: agent_urn.unwrap_or_default(),
        idempotency_key: Some(key.clone()),
        owner_principal: principal,
        source: None,
        credential_id: None,
    };
    let new = NewInboxRow {
        run_id: run_id.to_string(),
        mode: "QUEUE".into(),
        idempotency_key: key,
        owner_user_id: owner,
        text,
        thread_ref: first.thread_ref.clone(),
        workspace_id: first.workspace_id.clone(),
        queued_spec_json: Some(
            serde_json::to_string(&start).map_err(|e| OrchestratorError::Core(e.to_string()))?,
        ),
    };
    Ok((new, queued))
}

/// Sets `inputs.prompt` of a spec.
pub fn set_prompt(spec: &mut RunSpec, text: &str) {
    let inputs = spec.inputs.get_or_insert_with(Default::default);
    inputs.fields.insert(
        "prompt".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(text.to_string())),
        },
    );
}

/// The undelivered steers of ended runs become queued tasks (AR-3.8). A user stop took
/// its steers already; a run the user cancelled likewise. Steers the thread log holds are
/// delivered, not run again. Returns how many tasks were queued.
pub fn queue_ended_runs_steers(store: &SqliteWorkflowStore) -> usize {
    let runs = match store.ended_runs_with_pending_steers() {
        Ok(runs) => runs,
        Err(err) => {
            tracing::warn!("inbox: ended runs with steers not listed: {err}");
            return 0;
        }
    };
    let mut queued = 0;
    for run_id in runs {
        match queue_run_steers(store, &run_id) {
            Ok(true) => queued += 1,
            Ok(false) => {}
            Err(err) => tracing::warn!("inbox: steers of {run_id} not queued: {err}"),
        }
    }
    queued
}

fn queue_run_steers(store: &SqliteWorkflowStore, run_id: &str) -> Result<bool, OrchestratorError> {
    let result = store.get_run_result_json(run_id)?.unwrap_or_default();
    let stopped = serde_json::from_str::<serde_json::Value>(&result)
        .ok()
        .is_some_and(|v| v["reason"] == crate::session_end::STOPPED_REASON);
    let run_state = store.get_run_state(run_id)?;
    if stopped || run_state == "CANCELLED" {
        // The user ended the task: what it never took in is thrown away, as a stop does.
        discard_for_stop(store, run_id)?;
        return Ok(false);
    }
    let mut steers: Vec<InboxRow> = store
        .list_inbox_messages(run_id)?
        .into_iter()
        .filter(|r| r.mode == "STEER" && r.state == crate::inbox_store::STATE_PENDING)
        .collect();
    if steers.is_empty() {
        return Ok(false);
    }
    let logged = match logged_user_messages(store, run_id) {
        Ok(messages) => messages,
        Err(err) => {
            tracing::warn!("inbox: log of {run_id} not read, queueing its steers: {err}");
            Vec::new()
        }
    };
    let pairs: Vec<(i64, String)> = steers.iter().map(|r| (r.seq, r.text.clone())).collect();
    let already = steers_in_log(&logged, &pairs);
    if !already.is_empty() {
        let done = store.mark_steers_delivered_by_seq(run_id, &already)?;
        if !done.is_empty() {
            crate::trajectory::append_trajectory_event(
                store,
                run_id,
                TYPE_INBOX_DELIVERED,
                &serde_json::json!({ "seqs": done }),
            );
        }
        steers.retain(|r| !already.contains(&r.seq));
    }
    if steers.is_empty() {
        return Ok(false);
    }
    let (new, queued_run) = merged_task(store, run_id, &steers)?;
    let seqs: Vec<i64> = steers.iter().map(|r| r.seq).collect();
    match store.merge_steers_into_queued(run_id, &seqs, &new, &queued_run)? {
        Some(row) => {
            emit_received(store, run_id, row.seq, "QUEUE");
            Ok(true)
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox_store::NewInboxRow;
    use vam_spec::v1::{ExecutionLease, ResourceId};

    const SESSION: &str = "sess-1";
    const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-1";

    fn store_with_run() -> Arc<SqliteWorkflowStore> {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store
            .create_session(SESSION, None, "vm-1", "ws", "default")
            .unwrap();
        store
            .persist_run_spec_with_owner(
                RUN,
                "{}",
                Some(SESSION),
                "agent",
                Some("k"),
                "p",
                crate::users::LOCAL_USER_ID,
            )
            .unwrap();
        store.create_run(&ResourceId { urn: RUN.into() }).unwrap();
        store
    }

    fn lease() -> ExecutionLease {
        ExecutionLease {
            run_id: Some(ResourceId { urn: RUN.into() }),
            lifecycle_generation: 1,
            ..Default::default()
        }
    }

    fn put(store: &SqliteWorkflowStore, key: &str, text: &str) -> i64 {
        store
            .insert_inbox_message(&NewInboxRow {
                run_id: RUN.into(),
                mode: "STEER".into(),
                idempotency_key: key.into(),
                owner_user_id: "u1".into(),
                text: text.into(),
                thread_ref: "main".into(),
                workspace_id: "ws".into(),
                queued_spec_json: None,
            })
            .unwrap()
            .seq
    }

    /// A pull whose cursor and acknowledgement are the same seq (the end of a cycle).
    fn pull(store: &Arc<SqliteWorkflowStore>, after: i64) -> PullInboxResponse {
        pull_acked(store, after, after)
    }

    fn pull_acked(store: &Arc<SqliteWorkflowStore>, after: i64, ack: i64) -> PullInboxResponse {
        handle_pull_inbox(
            store,
            SESSION,
            &PullInboxRequest {
                lease: Some(lease()),
                after_seq: after,
                ack_seq: ack,
            },
        )
    }

    fn events(store: &SqliteWorkflowStore, kind: &str) -> Vec<serde_json::Value> {
        store
            .read_outbox_for_run(RUN, 0)
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == kind)
            .map(|e| serde_json::from_str(&e.payload).unwrap())
            .collect()
    }

    fn pairs(texts: &[&str]) -> Vec<(i64, String)> {
        texts
            .iter()
            .enumerate()
            .map(|(i, t)| (i as i64 + 1, t.to_string()))
            .collect()
    }

    #[test]
    fn steers_the_log_already_holds_are_found_and_the_rest_are_not() {
        let log = |messages: &[&str]| messages.iter().map(|m| m.to_string()).collect::<Vec<_>>();
        // The guest joined two steers into one message, then died before acknowledging.
        assert_eq!(
            steers_in_log(&log(&["one\n\ntwo"]), &pairs(&["one", "two", "three"])),
            vec![1, 2]
        );
        // One steer on its own.
        assert_eq!(
            steers_in_log(&log(&["one"]), &pairs(&["one", "two"])),
            vec![1]
        );
        // A message counts for one group only: the same text sent twice needs two messages.
        assert_eq!(
            steers_in_log(&log(&["same"]), &pairs(&["same", "same"])),
            vec![1]
        );
        assert_eq!(
            steers_in_log(&log(&["same", "same"]), &pairs(&["same", "same"])),
            vec![1, 2]
        );
        // Nothing logged, nothing found; a part of a longer message is not a match.
        assert!(steers_in_log(&[], &pairs(&["one"])).is_empty());
        assert!(steers_in_log(&log(&["one and more"]), &pairs(&["one"])).is_empty());
    }

    #[test]
    fn a_request_from_a_resolved_spec_keeps_the_ids_and_starts_fresh() {
        let mut resolved = RunSpec {
            workspace_ref_uri: "volume://sp_aaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            thread_ref: "th_bbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            space_id: "sp_aaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            thread_id: "th_bbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            inbox_mode: 2,
            ..Default::default()
        };
        resolved.run_id = Some(vam_spec::v1::ResourceId {
            urn: "urn:r".into(),
        });
        set_prompt(&mut resolved, "next");
        let request = request_from_resolved(&resolved, true);
        assert_eq!(
            request.workspace_ref_uri,
            "volume://sp_aaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(request.space_id, "sp_aaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(request.thread_id, "th_bbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert!(request.run_id.is_none());
        assert_eq!(request.inbox_mode, 0);
        assert_eq!(join_steers(["a", "b"]), "a\n\nb");
    }

    #[test]
    fn the_committed_log_gives_the_user_messages_after_the_tasks_own_prompt() {
        use vam_storage::thread_log::{
            LogMessage, NewRecord, ThreadStore, TreeThreadStore, WireMessage,
        };
        let dir = tempfile::tempdir().unwrap();
        let wm = vam_storage::open_workspace_manager_with(
            dir.path(),
            vam_storage::Drive9Backend::Directory,
        )
        .unwrap();
        let tree = wm
            .materialize(
                "s1",
                "ws",
                &vam_storage::StorageProfile::new("t".into(), 8 * 1024 * 1024),
                None,
                None,
                None,
            )
            .unwrap();
        let record = |run: &str, role: &str, text: &str| {
            let wire = WireMessage {
                role: role.into(),
                content: text.into(),
                tool_calls: vec![],
                tool_call_id: String::new(),
            };
            let mut rec = NewRecord::new(
                "message",
                "guest",
                serde_json::json!({ "msg": LogMessage::from_wire(&wire) }),
            );
            rec.run = run.into();
            rec
        };
        TreeThreadStore::new(&tree)
            .append(
                "main",
                vec![
                    record("run-a", "user", "the task"),
                    record("run-a", "assistant", "working"),
                    record("run-a", "user", "also add a title"),
                    record("run-b", "user", "another task"),
                ],
            )
            .unwrap();
        wm.commit_and_release("s1", "ws").unwrap();
        assert_eq!(
            logged_user_messages_in(wm.layout(), "ws", "main", "run-a"),
            vec!["also add a title".to_string()]
        );
        assert!(logged_user_messages_in(wm.layout(), "ws", "main", "run-c").is_empty());
        assert!(logged_user_messages_in(wm.layout(), "ws", "no-thread", "run-a").is_empty());
    }

    #[test]
    fn a_pull_returns_pending_steers_in_order_and_does_not_consume_them() {
        let store = store_with_run();
        put(&store, "a", "first");
        put(&store, "b", "second");
        let again = |s: &Arc<SqliteWorkflowStore>| {
            pull(s, 0)
                .messages
                .into_iter()
                .map(|m| (m.seq, m.text))
                .collect::<Vec<_>>()
        };
        let want = vec![(1, "first".to_string()), (2, "second".to_string())];
        assert_eq!(again(&store), want);
        assert_eq!(again(&store), want, "redelivered until acknowledged");
        assert!(events(&store, TYPE_INBOX_DELIVERED).is_empty());
    }

    #[test]
    fn the_next_pull_acknowledges_and_clears_the_text() {
        let store = store_with_run();
        put(&store, "a", "first");
        put(&store, "b", "second");
        assert_eq!(pull(&store, 2).messages.len(), 0);
        let delivered = events(&store, TYPE_INBOX_DELIVERED);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0]["seqs"], serde_json::json!([1, 2]));
        let rows = store.list_inbox_messages(RUN).unwrap();
        assert!(rows
            .iter()
            .all(|r| r.state == "delivered" && r.text.is_empty()));
        // A repeated acknowledgement says nothing more.
        assert_eq!(pull(&store, 2).messages.len(), 0);
        assert_eq!(events(&store, TYPE_INBOX_DELIVERED).len(), 1);
    }

    #[test]
    fn a_message_sent_after_an_ack_is_returned_next() {
        let store = store_with_run();
        put(&store, "a", "first");
        let _ = pull(&store, 1);
        put(&store, "b", "second");
        let got = pull(&store, 1).messages;
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].seq, got[0].text.as_str()), (2, "second"));
    }

    #[test]
    fn replies_are_bounded_and_always_make_progress() {
        let store = store_with_run();
        let big = "x".repeat(32 * 1024);
        for i in 0..20 {
            put(&store, &format!("k{i}"), &big);
        }
        let mut seen = Vec::new();
        let mut after = 0;
        let mut pulls = 0;
        loop {
            let reply = pull(&store, after);
            pulls += 1;
            assert!(reply.error.is_none());
            let bytes: usize = reply.messages.iter().map(|m| m.text.len()).sum();
            assert!(bytes <= PULL_REPLY_BUDGET, "{bytes}");
            let Some(last) = reply.messages.last().map(|m| m.seq) else {
                break;
            };
            seen.extend(reply.messages.iter().map(|m| m.seq));
            after = last;
            assert!(pulls <= 21);
        }
        assert_eq!(seen, (1..=20).collect::<Vec<_>>());
        assert_eq!(pulls, 21);
    }

    #[test]
    fn fetching_a_later_batch_never_acknowledges_an_unapplied_one() {
        let store = store_with_run();
        let big = "x".repeat(32 * 1024);
        for i in 0..3 {
            put(&store, &format!("k{i}"), &big);
        }
        // The guest has applied nothing (ack 0) and walks the batches: fetch 1, then 2.
        assert_eq!(pull_acked(&store, 0, 0).messages[0].seq, 1);
        assert_eq!(pull_acked(&store, 1, 0).messages[0].seq, 2);
        // It dies here, before appending anything. Nothing was acknowledged.
        let rows = store.list_inbox_messages(RUN).unwrap();
        assert!(rows.iter().all(|r| r.state == "pending" && r.text == big));
        assert!(events(&store, TYPE_INBOX_DELIVERED).is_empty());
        // A replacement guest gets all three again from the start.
        let again: Vec<i64> = pull_acked(&store, 0, 0)
            .messages
            .iter()
            .map(|m| m.seq)
            .collect();
        assert_eq!(again, vec![1]);
        // Only the acknowledgement of what it applied marks delivery.
        let _ = pull_acked(&store, 3, 2);
        let rows = store.list_inbox_messages(RUN).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.state.as_str()).collect::<Vec<_>>(),
            vec!["delivered", "delivered", "pending"]
        );
        assert_eq!(rows[2].text, big);
    }

    #[test]
    fn a_lease_for_another_run_is_refused() {
        let store = store_with_run();
        let mut other = lease();
        other.run_id = Some(ResourceId {
            urn: "urn:vam:run:local.vam.dev/acme/other".into(),
        });
        let reply = handle_pull_inbox(
            &store,
            SESSION,
            &PullInboxRequest {
                lease: Some(other),
                after_seq: 0,
                ack_seq: 0,
            },
        );
        assert!(reply.error.is_some() && reply.messages.is_empty());
    }

    #[test]
    fn queued_messages_are_never_pulled() {
        let store = store_with_run();
        store
            .insert_inbox_message(&NewInboxRow {
                run_id: RUN.into(),
                mode: "QUEUE".into(),
                idempotency_key: "q".into(),
                owner_user_id: "u1".into(),
                text: "later".into(),
                thread_ref: "main".into(),
                workspace_id: "ws".into(),
                queued_spec_json: None,
            })
            .unwrap();
        assert!(pull(&store, 0).messages.is_empty());
        assert_eq!(store.list_inbox_messages(RUN).unwrap()[0].state, "queued");
    }
}

#[cfg(test)]
mod queued_caller_tests {
    use super::*;

    #[test]
    fn a_caller_stored_before_triggers_still_reads_and_is_a_person() {
        let old = r#"{"key_id":"k","name":"n","principal":"p","scopes":[],"owner_user_id":"u","role":"admin","session_token":false}"#;
        let caller: QueuedCaller = serde_json::from_str(old).unwrap();
        assert!(caller.source.is_none() && caller.credential_id.is_none());
        assert!(caller.not_after.is_none());
    }

    #[test]
    fn only_a_trigger_records_a_source() {
        use crate::api::run_start::RunSource;
        let person = RunSource::Person {
            surface: "cli".into(),
        };
        assert_eq!(person.record(), None);
        let trigger = RunSource::Trigger {
            broker_id: "b9".into(),
            grant_id: "g".into(),
            trigger_ref: "t".into(),
            fire_at: 0,
            fire_kind: "cron".into(),
            surface: "api".into(),
        };
        assert_eq!(trigger.record().as_deref(), Some("broker:b9"));
        assert!(trigger.is_trigger() && !person.is_trigger());
    }

    #[test]
    fn queued_caller_owner_ignores_the_principal() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store.create_user("alice", "alice").unwrap();
        let queued = |key_id: &str, owner: &str| QueuedCaller {
            key_id: key_id.into(),
            name: "k".into(),
            principal: "User::\"bootstrap\"".into(),
            scopes: vec!["sessions:write".into()],
            owner_user_id: owner.into(),
            role: "member".into(),
            session_token: true,
            source: None,
            credential_id: None,
            not_after: None,
        };
        let member = queued("key-1", "alice").to_caller(&store).unwrap();
        assert_eq!(member.owner_user_id, "alice");
        let ownerless = queued("key-1", "").to_caller(&store).unwrap();
        assert_eq!(ownerless.owner_user_id, crate::users::LOCAL_USER_ID);
        let genuine = queued(crate::owner::BOOTSTRAP_KEY_ID, "")
            .to_caller(&store)
            .unwrap();
        assert_eq!(genuine.owner_user_id, crate::users::OPERATOR_USER_ID);
    }
}
