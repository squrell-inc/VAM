//! Host-authoritative intra-run trajectory events in the workflow outbox.
//!
//! The outbox is the source of truth; at run end a digest + compact summary is
//! persisted on the volume inside `ThreadTurnPayload` for portable replay.

use sha2::{Digest as Sha2Digest, Sha256};
use vam_storage::TrajectorySummary;

use crate::error::{OrchestratorError, OutboxEvent};
use crate::workflow::SqliteWorkflowStore;

pub const EVENT_TOOL_INVOKED: &str = "vam.v1.tool.invoked";
pub const EVENT_POLICY_DECIDED: &str = "vam.v1.policy.decided";
pub const EVENT_SKILL_LOADED: &str = "vam.v1.skill.loaded";
pub const EVENT_HOST_TOOL_COMPLETED: &str = "vam.v1.host_tool.completed";
/// Observational guest-tool catalog events (StreamLogs ingest). Not trajectory-authoritative.
pub const EVENT_TOOL_COMPLETED: &str = "vam.v1.tool.completed";
pub const EVENT_TOOL_FAILED: &str = "vam.v1.tool.failed";

pub const TRAJECTORY_EVENT_TYPES: &[&str] = vam_telemetry::TRAJECTORY_REPLAY_EVENT_TYPES;

const FS_PATH_TOOLS: &[&str] = &["read_file", "write_file", "edit_file", "delete_file"];
const MAX_TOUCHED_PATHS: usize = 32;

pub fn is_trajectory_event(event_type: &str) -> bool {
    TRAJECTORY_EVENT_TYPES.contains(&event_type)
}

pub fn extract_workspace_path(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    if !FS_PATH_TOOLS.contains(&tool_name) {
        return None;
    }
    let serde_json::Value::Object(map) = args else {
        return None;
    };
    map.get("path")
        .or_else(|| map.get("filepath"))
        .and_then(|v| v.as_str())
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

pub fn extract_skill_id(args: &serde_json::Value) -> Option<String> {
    let serde_json::Value::Object(map) = args else {
        return None;
    };
    map.get("skill")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn compute_trajectory_digest(events: &[OutboxEvent]) -> String {
    let mut hasher = Sha256::new();
    for event in events.iter().filter(|e| is_trajectory_event(&e.event_type)) {
        hasher.update(event.event_type.as_bytes());
        hasher.update([0u8]);
        hasher.update(event.payload.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

/// Serializes all trajectory events into canonical JSON bytes and persists the replay blob in CAS.
/// Returns the SHA-256 content address hash of the chunk in CAS.
pub fn persist_trajectory_replay_blob(
    events: &[OutboxEvent],
    chunk_store: &vam_storage::ChunkStore,
) -> Result<String, OrchestratorError> {
    let trajectory_events: Vec<&OutboxEvent> = events
        .iter()
        .filter(|e| is_trajectory_event(&e.event_type))
        .collect();

    let json_bytes = serde_json::to_vec(&trajectory_events).map_err(|e| {
        OrchestratorError::Core(format!("Failed to serialize trajectory replay blob: {}", e))
    })?;

    let hash = chunk_store.put_chunk(&json_bytes).map_err(|e| {
        OrchestratorError::Core(format!("Failed to store trajectory replay in CAS: {}", e))
    })?;

    Ok(hash)
}

pub fn summarize_trajectory(events: &[OutboxEvent]) -> TrajectorySummary {
    let trajectory: Vec<_> = events
        .iter()
        .filter(|e| is_trajectory_event(&e.event_type))
        .collect();

    let mut tool_names = Vec::new();
    let mut tool_seen = std::collections::HashSet::new();
    let mut touched_paths = Vec::new();
    let mut path_seen = std::collections::HashSet::new();
    let mut skill_ids = Vec::new();
    let mut skill_seen = std::collections::HashSet::new();

    for event in &trajectory {
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.payload) else {
            continue;
        };
        match event.event_type.as_str() {
            EVENT_POLICY_DECIDED => {
                if let Some(name) = payload.get("tool_name").and_then(|v| v.as_str()) {
                    if tool_seen.insert(name.to_string()) {
                        tool_names.push(name.to_string());
                    }
                }
                if let Some(path) = payload.get("path").and_then(|v| v.as_str()) {
                    if !path.is_empty() && path_seen.insert(path.to_string()) {
                        touched_paths.push(path.to_string());
                    }
                }
            }
            EVENT_SKILL_LOADED => {
                if let Some(skill) = payload.get("skill_id").and_then(|v| v.as_str()) {
                    if skill_seen.insert(skill.to_string()) {
                        skill_ids.push(skill.to_string());
                    }
                }
            }
            EVENT_HOST_TOOL_COMPLETED => {
                if let Some(relpath) = payload.get("artifact_relpath").and_then(|v| v.as_str()) {
                    if !relpath.is_empty() {
                        let host_path = format!(".vam/host-artifacts/{}", relpath);
                        if path_seen.insert(host_path.clone()) {
                            touched_paths.push(host_path);
                        }
                    }
                }
            }
            EVENT_TOOL_INVOKED => {
                if let Some(name) = payload.get("tool_name").and_then(|v| v.as_str()) {
                    if tool_seen.insert(name.to_string()) {
                        tool_names.push(name.to_string());
                    }
                }
                if let Some(path) = payload.get("path").and_then(|v| v.as_str()) {
                    if !path.is_empty() && path_seen.insert(path.to_string()) {
                        touched_paths.push(path.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    if touched_paths.len() > MAX_TOUCHED_PATHS {
        touched_paths.truncate(MAX_TOUCHED_PATHS);
    }

    TrajectorySummary {
        trajectory_digest: compute_trajectory_digest(events),
        touched_paths,
        skill_ids,
        tool_names,
    }
}

pub fn append_trajectory_event(
    store: &SqliteWorkflowStore,
    run_id: &str,
    event_type: &str,
    payload: &serde_json::Value,
) {
    if run_id.is_empty() {
        return;
    }
    let payload_str = payload.to_string();
    let _ = store.append_outbox_event(run_id, event_type, &payload_str);
}

/// Load the CAS trajectory replay for a finished run. 409 if no attested blob.
pub fn load_trajectory_replay(
    store: &SqliteWorkflowStore,
    chunk_store: &vam_storage::ChunkStore,
    run_id: &str,
) -> Result<vam_telemetry::TrajectoryReplay, OrchestratorError> {
    let _ = store.get_run_state(run_id)?;
    let events = store.read_outbox_for_run(run_id, 0)?;
    let attested = events
        .iter()
        .find(|e| e.event_type == "vam.v1.run.attested")
        .ok_or_else(|| {
            OrchestratorError::Conflict(format!(
                "run '{run_id}' has no trajectory blob; export after the run finishes"
            ))
        })?;
    let digest = trajectory_digest_from_attested(attested)?;
    if digest.is_empty() {
        return Err(OrchestratorError::Conflict(format!(
            "run '{run_id}' has no trajectory blob; export after the run finishes"
        )));
    }
    let bytes = chunk_store.get_chunk(&digest).map_err(|_| {
        OrchestratorError::Conflict(format!(
            "run '{run_id}' has no trajectory blob; export after the run finishes"
        ))
    })?;
    vam_telemetry::parse_trajectory_replay(digest, &bytes)
        .map_err(|e| OrchestratorError::Core(e.to_string()))
}

fn trajectory_digest_from_attested(event: &OutboxEvent) -> Result<String, OrchestratorError> {
    let envelope: vam_spec::v1::SignedEnvelope =
        vam_spec::encoding::json_to_proto("vam.v1.SignedEnvelope", &event.payload).map_err(
            |e| OrchestratorError::Core(format!("invalid vam.v1.run.attested envelope: {e}")),
        )?;
    let attestation: vam_telemetry::RunOutcomeAttestation =
        serde_json::from_slice(&envelope.claims)
            .map_err(|e| OrchestratorError::Core(format!("invalid attestation claims: {e}")))?;
    Ok(attestation.trajectory_digest)
}

#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
pub fn emit_tool_invoked(
    store: &SqliteWorkflowStore,
    run_id: &str,
    tool_name: &str,
    args_digest: &str,
    path: Option<&str>,
    capture: vam_telemetry::TelemetryCapture,
    chunk_store: Option<&vam_storage::ChunkStore>,
    redacted_args: Option<&serde_json::Value>,
) {
    let mut payload = serde_json::json!({
        "tool_name": tool_name,
        "args_digest": args_digest,
    });
    if let Some(p) = path.filter(|s| !s.is_empty()) {
        payload["path"] = serde_json::Value::String(p.to_string());
    }
    if let Some(args) = redacted_args {
        let bytes = args.to_string().into_bytes();
        let captured = crate::pep_capture::persist_redacted_bytes(capture, chunk_store, &bytes);
        if let Some(cas) = captured.cas_digest {
            payload["args_cas"] = serde_json::Value::String(cas);
        }
        if let Some(preview) = captured.preview {
            payload["args_preview"] = serde_json::Value::String(preview);
        }
    }
    append_trajectory_event(store, run_id, EVENT_TOOL_INVOKED, &payload);
    vam_telemetry::record_tool_invoked_signal(run_id, tool_name);
}

pub fn emit_policy_decided(
    store: &SqliteWorkflowStore,
    run_id: &str,
    tool_name: &str,
    disposition: &str,
    args_digest: &str,
    path: Option<&str>,
) {
    let mut payload = serde_json::json!({
        "tool_name": tool_name,
        "disposition": disposition,
        "args_digest": args_digest,
    });
    if let Some(p) = path.filter(|s| !s.is_empty()) {
        payload["path"] = serde_json::Value::String(p.to_string());
    }
    append_trajectory_event(store, run_id, EVENT_POLICY_DECIDED, &payload);

    if tool_name == "load_skill" && disposition == "ALLOW" {
        // skill.loaded is emitted separately when skill id is known
    }
}

pub fn emit_skill_loaded(store: &SqliteWorkflowStore, run_id: &str, skill_id: &str) {
    if skill_id.is_empty() {
        return;
    }
    append_trajectory_event(
        store,
        run_id,
        EVENT_SKILL_LOADED,
        &serde_json::json!({ "skill_id": skill_id }),
    );
}

pub fn emit_host_tool_completed(
    store: &SqliteWorkflowStore,
    run_id: &str,
    tool_name: &str,
    adapter_id: &str,
    artifact_relpath: &str,
) {
    append_trajectory_event(
        store,
        run_id,
        EVENT_HOST_TOOL_COMPLETED,
        &serde_json::json!({
            "tool_name": tool_name,
            "adapter_id": adapter_id,
            "artifact_relpath": artifact_relpath,
        }),
    );
}

/// Guest `tool.exec` catalog row. Observational — not added to `TRAJECTORY_EVENT_TYPES`.
pub fn emit_guest_tool_outcome(
    store: &SqliteWorkflowStore,
    run_id: &str,
    payload: &serde_json::Value,
    failed: bool,
) {
    let event_type = if failed {
        EVENT_TOOL_FAILED
    } else {
        EVENT_TOOL_COMPLETED
    };
    append_trajectory_event(store, run_id, event_type, payload);
}

/// One finished tool call on this run's trajectory.
///
/// `result` is `success` for `vam.v1.tool.completed` and `vam.v1.host_tool.completed`,
/// and `failed` for `vam.v1.tool.failed`. Playbook evidence matches `tool_name` and
/// `result` against calls whose outbox sequence is after the step became available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedToolCall {
    pub sequence: i64,
    pub event_type: String,
    pub tool_name: String,
    pub result: String,
}

/// Read completed and failed tool events from a run's outbox, in sequence order.
pub fn read_completed_tool_calls(
    connection: &rusqlite::Connection,
    run_id: &str,
) -> Result<Vec<CompletedToolCall>, OrchestratorError> {
    let mut statement = connection.prepare(
        "SELECT sequence, event_type, payload
         FROM workflow_outbox
         WHERE run_id = ?1
           AND event_type IN (?2, ?3, ?4)
         ORDER BY sequence ASC",
    )?;
    let rows = statement.query_map(
        rusqlite::params![
            run_id,
            EVENT_TOOL_COMPLETED,
            EVENT_TOOL_FAILED,
            EVENT_HOST_TOOL_COMPLETED
        ],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    )?;
    let mut calls = Vec::new();
    for row in rows {
        let (sequence, event_type, payload) = row.map_err(OrchestratorError::Database)?;
        if let Some(call) = completed_tool_call(sequence, &event_type, &payload) {
            calls.push(call);
        }
    }
    Ok(calls)
}

fn completed_tool_call(
    sequence: i64,
    event_type: &str,
    payload: &str,
) -> Option<CompletedToolCall> {
    let result = match event_type {
        EVENT_TOOL_COMPLETED | EVENT_HOST_TOOL_COMPLETED => "success",
        EVENT_TOOL_FAILED => "failed",
        _ => return None,
    };
    let payload: serde_json::Value = serde_json::from_str(payload).ok()?;
    let tool_name = payload.get("tool_name")?.as_str()?.trim();
    if tool_name.is_empty() {
        return None;
    }
    Some(CompletedToolCall {
        sequence,
        event_type: event_type.to_string(),
        tool_name: tool_name.to_string(),
        result: result.to_string(),
    })
}

/// Opaque ref stored on the step for one matching trajectory event.
pub fn evidence_ref(call: &CompletedToolCall) -> String {
    format!("{}:{}", call.event_type, call.sequence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::SessionSecurityContext;
    use crate::session::{builtin_tool_catalog, ToolCatalogEntry};
    use crate::workflow::SqliteWorkflowStore;
    use std::sync::Arc;
    use vam_inference::SessionBudget;
    use vam_security::PolicyMode;
    use vam_spec::v1::{tool_intent_response::Disposition, ToolIntentRequest};

    fn seed_sqlite_session(store: &SqliteWorkflowStore, session_id: &str) {
        store
            .create_session(session_id, None, "vm-test", "ws-test", "storage-default")
            .expect("create session");
        let mut catalog = builtin_tool_catalog();
        catalog.push(ToolCatalogEntry {
            tool_name: "query_db".to_string(),
            command: "skill".to_string(),
            entrypoint: "query_db".to_string(),
            digest: String::new(),
            side_effect: 1,
            skill_name: String::new(),
            approval_required: false,
        });
        store
            .record_tool_catalog(session_id, &catalog)
            .expect("seed catalog");
    }

    #[test]
    fn guest_tool_catalog_events_are_not_trajectory_authoritative() {
        assert!(!is_trajectory_event(EVENT_TOOL_COMPLETED));
        assert!(!is_trajectory_event(EVENT_TOOL_FAILED));
        assert!(is_trajectory_event(EVENT_HOST_TOOL_COMPLETED));
    }

    #[test]
    fn digest_stable_for_same_event_sequence() {
        let events = vec![
            OutboxEvent {
                sequence: 1,
                run_id: "run-1".into(),
                event_type: EVENT_POLICY_DECIDED.into(),
                payload: r#"{"tool_name":"write_file","disposition":"ALLOW","args_digest":"abc","path":"notes/hello.txt"}"#.into(),
            },
            OutboxEvent {
                sequence: 2,
                run_id: "run-1".into(),
                event_type: EVENT_SKILL_LOADED.into(),
                payload: r#"{"skill_id":"db-tools"}"#.into(),
            },
        ];
        let d1 = compute_trajectory_digest(&events);
        let d2 = compute_trajectory_digest(&events);
        assert_eq!(d1, d2);
        assert_eq!(d1.len(), 64);
    }

    #[test]
    fn summarize_collects_paths_skills_tools() {
        let events = vec![
            OutboxEvent {
                sequence: 1,
                run_id: "run-1".into(),
                event_type: EVENT_POLICY_DECIDED.into(),
                payload: r#"{"tool_name":"load_skill","disposition":"ALLOW","args_digest":"a1"}"#.into(),
            },
            OutboxEvent {
                sequence: 2,
                run_id: "run-1".into(),
                event_type: EVENT_SKILL_LOADED.into(),
                payload: r#"{"skill_id":"db-tools"}"#.into(),
            },
            OutboxEvent {
                sequence: 3,
                run_id: "run-1".into(),
                event_type: EVENT_POLICY_DECIDED.into(),
                payload: r#"{"tool_name":"write_file","disposition":"ALLOW","args_digest":"a2","path":"notes/hello.txt"}"#.into(),
            },
            OutboxEvent {
                sequence: 4,
                run_id: "run-1".into(),
                event_type: EVENT_POLICY_DECIDED.into(),
                payload: r#"{"tool_name":"query_db","disposition":"DENY","args_digest":"a3"}"#.into(),
            },
            OutboxEvent {
                sequence: 5,
                run_id: "run-1".into(),
                event_type: "vam.v1.run.created".into(),
                payload: "created".into(),
            },
        ];
        let summary = summarize_trajectory(&events);
        assert!(summary.tool_names.contains(&"load_skill".to_string()));
        assert!(summary.tool_names.contains(&"write_file".to_string()));
        assert!(summary.tool_names.contains(&"query_db".to_string()));
        assert_eq!(summary.skill_ids, vec!["db-tools".to_string()]);
        assert_eq!(summary.touched_paths, vec!["notes/hello.txt".to_string()]);
        assert!(!summary.trajectory_digest.is_empty());
    }

    #[test]
    fn pep_deny_and_allow_both_in_outbox() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-traj";
        seed_sqlite_session(&store, session_id);
        let ctx = SessionSecurityContext::new(
            session_id.to_string(),
            "urn:vam:agent:local.vam.dev/vam/sqlite-assistant".to_string(),
            include_str!("../../../examples/sqlite-assistant/policy.cedar"),
            PolicyMode::Enforce,
            1,
            ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]),
            None,
            Arc::new(SessionBudget::new(None)),
        )
        .expect("security context");

        let run_id = "urn:vam:run:traj-test";
        let lease = vam_spec::v1::ExecutionLease {
            run_id: Some(vam_spec::v1::ResourceId {
                urn: run_id.to_string(),
            }),
            step_id: "step-1".to_string(),
            attempt_id: Some(vam_spec::v1::ResourceId {
                urn: "urn:vam:attempt:1".to_string(),
            }),
            lease_id: Some(vam_spec::v1::ResourceId {
                urn: "urn:vam:lease:1".to_string(),
            }),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: Some(prost_types::Timestamp {
                seconds: 4_102_444_800,
                nanos: 0,
            }),
            idempotency_key: "idem".to_string(),
        };

        let mut allow_args = prost_types::Struct::default();
        allow_args.fields.insert(
            "query".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "SELECT 1".to_string(),
                )),
            },
        );
        let allow_resp = ctx.handle_tool_intent_request(
            &store,
            ToolIntentRequest {
                lease: Some(lease.clone()),
                tool_name: "query_db".to_string(),
                arguments: Some(allow_args),
                arguments_digest: None,
                script_digest: None,
                call_id: String::new(),
            },
        );
        assert_eq!(allow_resp.disposition, Disposition::Authorized as i32);

        let mut deny_args = prost_types::Struct::default();
        deny_args.fields.insert(
            "query".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "DROP TABLE facts".to_string(),
                )),
            },
        );
        let deny_resp = ctx.handle_tool_intent_request(
            &store,
            ToolIntentRequest {
                lease: Some(lease),
                tool_name: "query_db".to_string(),
                arguments: Some(deny_args),
                arguments_digest: None,
                script_digest: None,
                call_id: String::new(),
            },
        );
        assert_eq!(deny_resp.disposition, Disposition::Denied as i32);
        assert!(
            vam_telemetry::VamMetricsRegistry::global()
                .get_counter(
                    vam_telemetry::METRIC_SECURITY_DENIED_TOTAL,
                    &[("status", "denied")]
                )
                .unwrap_or(0)
                >= 1,
            "Cedar DENY must increment vamd.security.denied_actions.total"
        );

        let events = store.read_outbox_for_run(run_id, 0).expect("outbox");
        let decided: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == EVENT_POLICY_DECIDED)
            .collect();
        assert_eq!(decided.len(), 2);
        assert!(decided.iter().any(|e| e.payload.contains("\"ALLOW\"")));
        assert!(decided.iter().any(|e| e.payload.contains("\"DENY\"")));
        assert!(!decided.iter().any(|e| e.payload.contains("DROP TABLE")));

        let chain = ctx.audit_ledger.get_chain().unwrap();
        assert!(chain.iter().all(|r| !r.context_digest.is_empty()));
    }

    #[test]
    fn same_tool_loop_increments_guardrail_once() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let run_id = "urn:vam:run:same-tool-loop";
        let before = vam_telemetry::VamMetricsRegistry::global()
            .get_counter(
                vam_telemetry::METRIC_GUARDRAIL_SIGNALS_TOTAL,
                &[("status", "same_tool_loop")],
            )
            .unwrap_or(0);
        for _ in 0..=vam_telemetry::SAME_TOOL_LOOP_THRESHOLD {
            emit_tool_invoked(
                &store,
                run_id,
                "bash",
                "digest",
                None,
                vam_telemetry::TelemetryCapture::None,
                None,
                None,
            );
        }
        let after = vam_telemetry::VamMetricsRegistry::global()
            .get_counter(
                vam_telemetry::METRIC_GUARDRAIL_SIGNALS_TOTAL,
                &[("status", "same_tool_loop")],
            )
            .unwrap_or(0);
        assert!(
            after > before,
            "same-tool loop should increment once: before={before} after={after}"
        );
        emit_tool_invoked(
            &store,
            run_id,
            "bash",
            "digest",
            None,
            vam_telemetry::TelemetryCapture::None,
            None,
            None,
        );
        let again = vam_telemetry::VamMetricsRegistry::global()
            .get_counter(
                vam_telemetry::METRIC_GUARDRAIL_SIGNALS_TOTAL,
                &[("status", "same_tool_loop")],
            )
            .unwrap_or(0);
        assert_eq!(again, after, "same-tool loop must fire once per run");
    }

    #[test]
    fn completed_tool_call_maps_success_and_failure() {
        let success = completed_tool_call(
            4,
            EVENT_TOOL_COMPLETED,
            r#"{"tool_name":"lookup","exit_code":0}"#,
        )
        .expect("success");
        assert_eq!(success.tool_name, "lookup");
        assert_eq!(success.result, "success");
        assert_eq!(evidence_ref(&success), "vam.v1.tool.completed:4");

        let host = completed_tool_call(
            5,
            EVENT_HOST_TOOL_COMPLETED,
            r#"{"tool_name":"echo","adapter_id":"echo"}"#,
        )
        .expect("host");
        assert_eq!(host.result, "success");
        assert_eq!(evidence_ref(&host), "vam.v1.host_tool.completed:5");

        let failed = completed_tool_call(
            6,
            EVENT_TOOL_FAILED,
            r#"{"tool_name":"lookup","exit_code":1}"#,
        )
        .expect("failed");
        assert_eq!(failed.result, "failed");

        assert!(completed_tool_call(7, EVENT_TOOL_INVOKED, r#"{"tool_name":"lookup"}"#).is_none());
        assert!(completed_tool_call(8, EVENT_TOOL_COMPLETED, r#"{"exit_code":0}"#).is_none());
    }
}
