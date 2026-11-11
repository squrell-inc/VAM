use ed25519_dalek::SigningKey;
use prost_types::Struct;
use std::sync::Arc;
use vam_inference::SessionBudget;
use vam_orchestrator::security::SessionSecurityContext;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_security::PolicyMode;
use vam_spec::v1::{
    tool_intent_response::Disposition, Cost, Digest, ExecutionLease, ResourceId, ToolIntentRequest,
};

fn create_test_lease(_session_id: &str) -> ExecutionLease {
    ExecutionLease {
        run_id: Some(ResourceId {
            urn: "urn:vam:run:local.vam.dev/acme/live-1".to_string(),
        }),
        step_id: "step-live-1".to_string(),
        attempt_id: Some(ResourceId {
            urn: "urn:vam:attempt:local.vam.dev/acme/live-1".to_string(),
        }),
        lease_id: Some(ResourceId {
            urn: format!("urn:vam:lease:local.vam.dev/acme/{_session_id}"),
        }),
        lifecycle_generation: 1,
        fencing_token: vec![1, 2, 3],
        expires_at: Some(prost_types::Timestamp {
            seconds: 4_102_444_800,
            nanos: 0,
        }),
        idempotency_key: "idem-live-1".to_string(),
    }
}

#[test]
fn test_live_sqlite_agent_flow() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let session_id = "session-sqlite-live".to_string();
    assert!(store
        .create_session(&session_id, None, "vm-1", "sqlite-assistant", "storage-1")
        .is_ok());
    let agent_urn = "urn:vam:agent:local.vam.dev/vam/sqlite-assistant".to_string();
    let policy_src = include_str!("../../../examples/sqlite-assistant/policy.cedar");
    let host_key = SigningKey::from_bytes(&[12u8; 32]);
    let budget = Arc::new(SessionBudget::new(Some(Cost {
        currency: "USD".to_string(),
        micro_amount: 500_000,
    })));

    let sec_ctx = SessionSecurityContext::new(
        session_id.clone(),
        agent_urn,
        policy_src,
        PolicyMode::Enforce,
        1,
        host_key,
        None,
        budget,
    )
    .unwrap();

    let catalog = vec![
        vam_orchestrator::ToolCatalogEntry {
            tool_name: "query_db".to_string(),
            command: "python".to_string(),
            entrypoint: "query.py".to_string(),
            digest: "d1".to_string(),
            side_effect: 1,
            skill_name: String::new(),
            approval_required: false,
        },
        vam_orchestrator::ToolCatalogEntry {
            tool_name: "insert_fact".to_string(),
            command: "python".to_string(),
            entrypoint: "insert.py".to_string(),
            digest: "d2".to_string(),
            side_effect: 2,
            skill_name: String::new(),
            approval_required: false,
        },
    ];
    store.record_tool_catalog(&session_id, &catalog).unwrap();

    // 1. SELECT query -> ALLOW
    let mut args_select = Struct::default();
    args_select.fields.insert(
        "query".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "SELECT count(*) FROM items".to_string(),
            )),
        },
    );

    let req_select = ToolIntentRequest {
        lease: Some(create_test_lease(&session_id)),
        tool_name: "query_db".to_string(),
        arguments: Some(args_select),
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "d1".to_string(),
        }),
        call_id: String::new(),
    };

    let resp_select = sec_ctx.handle_tool_intent_request(&store, req_select);
    assert_eq!(resp_select.disposition, Disposition::Authorized as i32);
    assert!(
        resp_select.capability_token.is_some(),
        "Authorized tool must return capability token"
    );

    // 2. DROP TABLE -> DENY
    let mut args_drop = Struct::default();
    args_drop.fields.insert(
        "query".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "DROP TABLE items".to_string(),
            )),
        },
    );

    let req_drop = ToolIntentRequest {
        lease: Some(create_test_lease(&session_id)),
        tool_name: "query_db".to_string(),
        arguments: Some(args_drop),
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "d1".to_string(),
        }),
        call_id: String::new(),
    };

    let resp_drop = sec_ctx.handle_tool_intent_request(&store, req_drop);
    assert_eq!(resp_drop.disposition, Disposition::Denied as i32);
    assert!(resp_drop.capability_token.is_none());

    // 3. Verify decisions logged in AuditLedger
    let chain = sec_ctx.audit_ledger.get_chain().unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].decision, "ALLOW");
    assert_eq!(chain[1].decision, "DENY");
}

#[test]
fn test_live_chaos_agent_flow() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let session_id = "session-chaos-live".to_string();
    assert!(store
        .create_session(&session_id, None, "vm-2", "chaos-assistant", "storage-1")
        .is_ok());
    let agent_urn = "urn:vam:agent:local.vam.dev/vam/chaos-assistant".to_string();
    let policy_src = include_str!("../../../examples/chaos-assistant/policy.cedar");
    let host_key = SigningKey::from_bytes(&[13u8; 32]);
    let budget = Arc::new(SessionBudget::new(None));

    let sec_ctx = SessionSecurityContext::new(
        session_id.clone(),
        agent_urn,
        policy_src,
        PolicyMode::Enforce,
        1,
        host_key,
        None,
        budget,
    )
    .unwrap();

    let catalog = vec![
        vam_orchestrator::ToolCatalogEntry {
            tool_name: "probe_sandbox".to_string(),
            command: "python".to_string(),
            entrypoint: "probe.py".to_string(),
            digest: "d1".to_string(),
            side_effect: 0, // SideEffect::None (Pure)
            skill_name: String::new(),
            approval_required: false,
        },
        vam_orchestrator::ToolCatalogEntry {
            tool_name: "check_env".to_string(),
            command: "python".to_string(),
            entrypoint: "env.py".to_string(),
            digest: "d2".to_string(),
            side_effect: 0,
            skill_name: String::new(),
            approval_required: false,
        },
        vam_orchestrator::ToolCatalogEntry {
            tool_name: "external_mutation".to_string(),
            command: "python".to_string(),
            entrypoint: "mutate.py".to_string(),
            digest: "d3".to_string(),
            side_effect: 3, // SideEffect::MutableExternal
            skill_name: String::new(),
            approval_required: false,
        },
    ];
    store.record_tool_catalog(&session_id, &catalog).unwrap();

    // 1. probe_sandbox -> ALLOW
    let req_probe = ToolIntentRequest {
        lease: Some(create_test_lease(&session_id)),
        tool_name: "probe_sandbox".to_string(),
        arguments: None,
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "d1".to_string(),
        }),
        call_id: String::new(),
    };
    let resp_probe = sec_ctx.handle_tool_intent_request(&store, req_probe);
    assert_eq!(resp_probe.disposition, Disposition::Authorized as i32);

    // 2. external_mutation -> DENIED by forbid rule against SideEffectClass 3
    let req_mut = ToolIntentRequest {
        lease: Some(create_test_lease(&session_id)),
        tool_name: "external_mutation".to_string(),
        arguments: None,
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "d3".to_string(),
        }),
        call_id: String::new(),
    };
    let resp_mut = sec_ctx.handle_tool_intent_request(&store, req_mut);
    assert_eq!(resp_mut.disposition, Disposition::Denied as i32);
}

#[test]
fn test_live_jira_agent_flow() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let session_id = "session-jira-live".to_string();
    assert!(store
        .create_session(&session_id, None, "vm-3", "jira-assistant", "storage-1")
        .is_ok());
    let agent_urn = "urn:vam:agent:local.vam.dev/vam/jira-assistant".to_string();
    let policy_src = include_str!("../../../examples/jira-assistant/policy.cedar");
    let host_key = SigningKey::from_bytes(&[14u8; 32]);
    let budget = Arc::new(SessionBudget::new(None));

    let sec_ctx = SessionSecurityContext::new(
        session_id.clone(),
        agent_urn,
        policy_src,
        PolicyMode::Enforce,
        1,
        host_key,
        None,
        budget,
    )
    .unwrap();

    let catalog = vec![
        vam_orchestrator::ToolCatalogEntry {
            tool_name: "jira_search_issues".to_string(),
            command: "python".to_string(),
            entrypoint: "jira.py".to_string(),
            digest: "d1".to_string(),
            side_effect: 1,
            skill_name: String::new(),
            approval_required: false,
        },
        vam_orchestrator::ToolCatalogEntry {
            tool_name: "jira_create_issue".to_string(),
            command: "python".to_string(),
            entrypoint: "create.py".to_string(),
            digest: "d2".to_string(),
            side_effect: 3,
            skill_name: String::new(),
            approval_required: false,
        },
    ];
    store.record_tool_catalog(&session_id, &catalog).unwrap();

    let run_id = vam_spec::v1::ResourceId {
        urn: "urn:vam:run:local.vam.dev/acme/live-1".to_string(),
    };
    store.create_run(&run_id).expect("run");
    let lease = create_test_lease(&session_id);
    store.issue_lease(&lease).expect("lease");

    // 1. Search issues -> ALLOW
    let req_read = ToolIntentRequest {
        lease: Some(create_test_lease(&session_id)),
        tool_name: "jira_search_issues".to_string(),
        arguments: None,
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "d1".to_string(),
        }),
        call_id: String::new(),
    };
    let resp_read = sec_ctx.handle_tool_intent_request(&store, req_read);
    assert_eq!(resp_read.disposition, Disposition::Authorized as i32);

    // 2. Create issue -> DENIED without HITL approval context
    let req_del = ToolIntentRequest {
        lease: Some(create_test_lease(&session_id)),
        tool_name: "jira_create_issue".to_string(),
        arguments: None,
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "d2".to_string(),
        }),
        call_id: String::new(),
    };
    let resp_del = sec_ctx.handle_tool_intent_request(&store, req_del);
    assert_eq!(resp_del.disposition, Disposition::Suspended as i32);
    assert!(resp_del.approval_request_id.is_some());
}

#[test]
fn test_live_sqlite_hitl_overlay_flow() {
    use vam_orchestrator::security::derive_hitl_overlay;

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let session_id = "session-sqlite-hitl-live".to_string();
    assert!(store
        .create_session(&session_id, None, "vm-4", "sqlite-assistant", "storage-1")
        .is_ok());
    let agent_urn = "urn:vam:agent:local.vam.dev/vam/sqlite-assistant".to_string();
    let overlay = derive_hitl_overlay(&agent_urn, &["insert_fact".to_string()]);
    let policy_src = format!(
        "{}\n\n{overlay}",
        include_str!("../../../examples/sqlite-assistant/policy.cedar")
    );
    let host_key = SigningKey::from_bytes(&[15u8; 32]);
    let budget = Arc::new(SessionBudget::new(None));

    let sec_ctx = SessionSecurityContext::new(
        session_id.clone(),
        agent_urn,
        &policy_src,
        PolicyMode::Enforce,
        1,
        host_key,
        None,
        budget,
    )
    .unwrap();

    let catalog = vec![vam_orchestrator::ToolCatalogEntry {
        tool_name: "insert_fact".to_string(),
        command: "python".to_string(),
        entrypoint: "insert.py".to_string(),
        digest: "d-insert".to_string(),
        side_effect: 2,
        skill_name: String::new(),
        approval_required: false,
    }];
    store.record_tool_catalog(&session_id, &catalog).unwrap();

    let run_id = ResourceId {
        urn: "urn:vam:run:local.vam.dev/acme/live-1".to_string(),
    };
    store.create_run(&run_id).expect("run");
    let lease = create_test_lease(&session_id);
    store.issue_lease(&lease).expect("lease");

    let req = ToolIntentRequest {
        lease: Some(create_test_lease(&session_id)),
        tool_name: "insert_fact".to_string(),
        arguments: None,
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "d-insert".to_string(),
        }),
        call_id: String::new(),
    };
    let resp = sec_ctx.handle_tool_intent_request(&store, req);
    assert_eq!(resp.disposition, Disposition::Suspended as i32);
    assert!(resp.approval_request_id.is_some());
}
