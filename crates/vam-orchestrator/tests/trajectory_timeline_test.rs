use std::sync::Arc;

use ed25519_dalek::SigningKey;
use prost_types::Struct;
use vam_inference::SessionBudget;
use vam_orchestrator::host_tools::HostToolRegistry;
use vam_orchestrator::security::SessionSecurityContext;
use vam_orchestrator::session::{builtin_tool_catalog, ToolCatalogEntry};
use vam_orchestrator::trajectory::{
    summarize_trajectory, EVENT_HOST_TOOL_COMPLETED, EVENT_POLICY_DECIDED, EVENT_SKILL_LOADED,
};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_security::PolicyMode;
use vam_spec::v1::ExecuteHostToolRequest;
use vam_spec::v1::{
    tool_intent_response::Disposition, ExecutionLease, ResourceId, ToolIntentRequest,
};
use vam_spec::validation::{builtin_tool_digest, host_tool_digest};
use vam_storage::{append_turn, read_timeline, replay_prompt, TrajectorySummary};

fn test_lease(run_id: &str) -> ExecutionLease {
    ExecutionLease {
        run_id: Some(ResourceId {
            urn: run_id.to_string(),
        }),
        step_id: "step-1".to_string(),
        attempt_id: Some(ResourceId {
            urn: "urn:vam:attempt:1".to_string(),
        }),
        lease_id: Some(ResourceId {
            urn: "urn:vam:lease:1".to_string(),
        }),
        lifecycle_generation: 1,
        fencing_token: vec![1],
        expires_at: Some(prost_types::Timestamp {
            seconds: 4_102_444_800,
            nanos: 0,
        }),
        idempotency_key: "idem".to_string(),
    }
}

#[test]
fn load_skill_emits_skill_loaded_outbox_event() {
    let store = SqliteWorkflowStore::in_memory().expect("store");
    let session_id = "sess-load-skill";
    store
        .create_session(session_id, None, "vm-test", "ws-test", "storage-default")
        .expect("create session");
    store
        .record_tool_catalog(session_id, &builtin_tool_catalog())
        .expect("seed catalog");

    let run_id = "urn:vam:run:skill-load";
    let ctx = SessionSecurityContext::new(
        session_id.to_string(),
        "urn:vam:agent:bun-assistant".to_string(),
        include_str!("../../../examples/bun-assistant/policy.cedar"),
        PolicyMode::Enforce,
        1,
        SigningKey::from_bytes(&[4u8; 32]),
        None,
        Arc::new(SessionBudget::new(None)),
    )
    .expect("security context")
    .with_effect_key_secret(vam_orchestrator::signing::NodeKeys::ephemeral().effect_key_secret());

    let mut args = Struct::default();
    args.fields.insert(
        "skill".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "db-tools".to_string(),
            )),
        },
    );

    let resp = ctx.handle_tool_intent_request(
        &store,
        ToolIntentRequest {
            lease: Some(test_lease(run_id)),
            tool_name: "load_skill".to_string(),
            arguments: Some(args),
            arguments_digest: None,
            script_digest: Some(vam_spec::v1::Digest {
                algorithm: 1,
                hex_hash: builtin_tool_digest("load_skill"),
            }),
            call_id: String::new(),
        },
    );
    assert_eq!(resp.disposition, Disposition::Authorized as i32);

    let events = store.read_outbox_for_run(run_id, 0).expect("outbox");
    assert!(events.iter().any(|e| e.event_type == EVENT_SKILL_LOADED));
    assert!(events.iter().any(|e| e.event_type == EVENT_POLICY_DECIDED));
}

#[tokio::test]
async fn host_echo_emits_host_tool_completed() {
    let store = SqliteWorkflowStore::in_memory().expect("store");
    let session_id = "sess-host-echo";
    store
        .create_session(session_id, None, "vm-test", "ws-test", "storage-default")
        .expect("create session");
    let mut catalog = builtin_tool_catalog();
    catalog.push(ToolCatalogEntry {
        tool_name: "host_echo".to_string(),
        command: "host".to_string(),
        entrypoint: "echo".to_string(),
        digest: host_tool_digest("echo"),
        side_effect: 1,
        skill_name: String::new(),
        approval_required: false,
    });
    store
        .record_tool_catalog(session_id, &catalog)
        .expect("seed catalog");

    let run_id = "urn:vam:run:host-echo";
    let host_echo_policy = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource
)
when {
    [
        Tool::"load_skill",
        Tool::"host_echo"
    ].contains(resource)
};
"#;
    let ctx = SessionSecurityContext::new(
        session_id.to_string(),
        "urn:vam:agent:bun-assistant".to_string(),
        host_echo_policy,
        PolicyMode::Enforce,
        1,
        SigningKey::from_bytes(&[5u8; 32]),
        None,
        Arc::new(SessionBudget::new(None)),
    )
    .expect("security context")
    .with_effect_key_secret(vam_orchestrator::signing::NodeKeys::ephemeral().effect_key_secret());

    let token = ctx
        .handle_tool_intent_request(
            &store,
            ToolIntentRequest {
                lease: Some(test_lease(run_id)),
                tool_name: "host_echo".to_string(),
                arguments: Some(Struct::default()),
                arguments_digest: None,
                script_digest: Some(vam_spec::v1::Digest {
                    algorithm: 1,
                    hex_hash: host_tool_digest("echo"),
                }),
                call_id: String::new(),
            },
        )
        .capability_token
        .expect("authorized");

    let registry = HostToolRegistry::with_test_adapters();
    let skills_dir = std::env::temp_dir().join(format!("vam-traj-skills-{}", std::process::id()));
    let resp = ctx
        .handle_execute_host_tool(
            &store,
            None,
            &registry,
            &skills_dir,
            ExecuteHostToolRequest {
                lease: Some(test_lease(run_id)),
                tool_name: "host_echo".to_string(),
                arguments: None,
                capability_token: Some(token),
                call_id: String::new(),
            },
        )
        .await;
    assert!(resp.error.is_none());

    let events = store.read_outbox_for_run(run_id, 0).expect("outbox");
    assert!(events
        .iter()
        .any(|e| e.event_type == EVENT_HOST_TOOL_COMPLETED));
}

#[test]
fn write_file_path_in_turn_summary_and_replay() {
    let dir = tempfile::tempdir().unwrap();
    let mat = dir.path();
    let thread = "main";
    let run_id = "urn:vam:run:write-path";

    let events = vec![
        vam_orchestrator::OutboxEvent {
            sequence: 1,
            run_id: run_id.to_string(),
            event_type: EVENT_POLICY_DECIDED.to_string(),
            payload: r#"{"tool_name":"write_file","disposition":"ALLOW","args_digest":"d1","path":"notes/hello.txt"}"#
                .to_string(),
        },
    ];
    let summary = summarize_trajectory(&events);
    assert_eq!(summary.touched_paths, vec!["notes/hello.txt".to_string()]);

    append_turn(
        mat,
        thread,
        run_id,
        "write hello",
        "wrote the note",
        "mock-model",
        "manifest-hash",
        None,
        Some(&summary),
    )
    .unwrap();

    let timeline = read_timeline(mat, thread).unwrap();
    let replayed = replay_prompt(&timeline, "what did you write?", 10);
    assert!(replayed.contains("notes/hello.txt"));
    assert!(replayed.contains("wrote the note"));
    assert!(replayed.contains("what did you write?"));
    assert!(replayed.contains("PLAN.md"));
    assert!(!replayed.contains("tampered observation"));
}

#[test]
fn tampered_guest_observations_do_not_affect_replay() {
    let dir = tempfile::tempdir().unwrap();
    let mat = dir.path();
    let thread = "main";
    let summary = TrajectorySummary {
        trajectory_digest: "deadbeef".repeat(8),
        touched_paths: vec!["notes/hello.txt".into()],
        skill_ids: vec![],
        tool_names: vec!["write_file".into()],
    };
    append_turn(
        mat,
        thread,
        "urn:vam:run:obs-test",
        "hello",
        "done",
        "mock",
        "mh",
        None,
        Some(&summary),
    )
    .unwrap();

    let agent_dir = mat.join(".vam/agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("observations.jsonl"),
        r#"{"tool":"write_file","disposition":"completed","summary":"tampered observation"}"#,
    )
    .unwrap();

    let timeline = read_timeline(mat, thread).unwrap();
    let replayed = replay_prompt(&timeline, "follow up", 10);
    assert!(replayed.contains("notes/hello.txt"));
    assert!(replayed.contains("PLAN.md"));
    assert!(!replayed.contains("tampered observation"));
    assert_eq!(
        timeline[0].trajectory_digest.as_deref(),
        Some(summary.trajectory_digest.as_str())
    );
}
