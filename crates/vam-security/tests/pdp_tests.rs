use ed25519_dalek::SigningKey;
use vam_security::{
    mint_capability_token, verify_capability_token, ApprovalSigner, ArcSwapPolicyEngine,
    AuditLedger, CapabilityClaims, CedarPolicyEngine, DecisionOutcome, EvaluationContext,
    EvaluationRequest, InMemoryReplayStore, PolicyDecisionPoint, PolicyMode,
};

const SQL_POLICY: &str = r#"
permit (
    principal == Agent::"urn:vam:agent:sqlite-assistant",
    action == Action::"ExecuteTool",
    resource == Tool::"query_db"
)
when {
    context has query &&
    context.query like "*SELECT*" &&
    !(context.query like "*DROP*") &&
    !(context.query like "*DELETE*")
};
"#;

const WRITE_CAP_POLICY: &str = r#"
permit (
    principal,
    action == Action::"WriteFile",
    resource
)
when {
    context has file_size_bytes &&
    context.file_size_bytes <= 10485760
};

forbid (
    principal,
    action == Action::"WriteFile",
    resource
)
when {
    context has file_size_bytes &&
    context.file_size_bytes > 10485760
};
"#;

const FS_POLICY: &str = r#"
permit (
    principal,
    action == Action::"ReadFile",
    resource
)
when {
    context has path &&
    (context.path like "/mnt/drive9/workspace/*" ||
     context.path like "/mnt/drive9/skills/*")
};

permit (
    principal,
    action == Action::"WriteFile",
    resource
)
when {
    context has path &&
    context.path like "/mnt/drive9/workspace/*" &&
    !(context.path like "/mnt/drive9/skills/*") &&
    context has file_size_bytes &&
    context.file_size_bytes <= 10485760
};

forbid (
    principal,
    action == Action::"WriteFile",
    resource
)
when {
    context has path && context.path like "/mnt/drive9/skills/*"
};
"#;

#[test]
fn test_read_file_skills_and_workspace_paths() {
    let engine = CedarPolicyEngine::from_default_schema(FS_POLICY, PolicyMode::Enforce, 1)
        .expect("Policy compilation should succeed");

    let mut skills_read = EvaluationRequest {
        principal: "Agent::\"urn:vam:agent:reader\"".to_string(),
        action: "Action::\"ReadFile\"".to_string(),
        resource: "Path::\"/mnt/drive9/skills/db-tools/SCHEMA.md\"".to_string(),
        context: EvaluationContext::default(),
        entities: Vec::new(),
    };
    skills_read.context.path = Some("/mnt/drive9/skills/db-tools/SCHEMA.md".to_string());
    assert!(engine.evaluate(&skills_read).is_allowed());

    let mut workspace_read = EvaluationRequest {
        principal: "Agent::\"urn:vam:agent:reader\"".to_string(),
        action: "Action::\"ReadFile\"".to_string(),
        resource: "Path::\"/mnt/drive9/workspace/notes/hello.txt\"".to_string(),
        context: EvaluationContext::default(),
        entities: Vec::new(),
    };
    workspace_read.context.path = Some("/mnt/drive9/workspace/notes/hello.txt".to_string());
    assert!(engine.evaluate(&workspace_read).is_allowed());
}

#[test]
fn test_write_file_skills_forbidden_workspace_allowed() {
    let engine = CedarPolicyEngine::from_default_schema(FS_POLICY, PolicyMode::Enforce, 1)
        .expect("Policy compilation should succeed");

    let mut skills_write = EvaluationRequest {
        principal: "Agent::\"urn:vam:agent:writer\"".to_string(),
        action: "Action::\"WriteFile\"".to_string(),
        resource: "Path::\"/mnt/drive9/skills/db-tools/evil.txt\"".to_string(),
        context: EvaluationContext::default(),
        entities: Vec::new(),
    };
    skills_write.context.path = Some("/mnt/drive9/skills/db-tools/evil.txt".to_string());
    skills_write.context.file_size_bytes = Some(4);
    assert!(engine.evaluate(&skills_write).is_denied());

    let mut workspace_write = EvaluationRequest {
        principal: "Agent::\"urn:vam:agent:writer\"".to_string(),
        action: "Action::\"WriteFile\"".to_string(),
        resource: "Path::\"/mnt/drive9/workspace/notes/hello.txt\"".to_string(),
        context: EvaluationContext::default(),
        entities: Vec::new(),
    };
    workspace_write.context.path = Some("/mnt/drive9/workspace/notes/hello.txt".to_string());
    workspace_write.context.file_size_bytes = Some(128);
    assert!(engine.evaluate(&workspace_write).is_allowed());
}

#[test]
fn test_sql_policy_allow_select_and_deny_drop() {
    let engine = CedarPolicyEngine::from_default_schema(SQL_POLICY, PolicyMode::Enforce, 1)
        .expect("Policy compilation should succeed");

    // 1. SELECT query -> ALLOW
    let mut req_select = EvaluationRequest {
        principal: "Agent::\"urn:vam:agent:sqlite-assistant\"".to_string(),
        action: "Action::\"ExecuteTool\"".to_string(),
        resource: "Tool::\"query_db\"".to_string(),
        context: EvaluationContext::default(),
        entities: Vec::new(),
    };
    req_select.context.query = Some("SELECT * FROM facts".to_string());

    let outcome_select = engine.evaluate(&req_select);
    assert!(
        outcome_select.is_allowed(),
        "SELECT query must be permitted"
    );

    // 2. DROP query -> DENY
    let mut req_drop = EvaluationRequest {
        principal: "Agent::\"urn:vam:agent:sqlite-assistant\"".to_string(),
        action: "Action::\"ExecuteTool\"".to_string(),
        resource: "Tool::\"query_db\"".to_string(),
        context: EvaluationContext::default(),
        entities: Vec::new(),
    };
    req_drop.context.query = Some("DROP TABLE facts".to_string());

    let outcome_drop = engine.evaluate(&req_drop);
    assert!(outcome_drop.is_denied(), "DROP query must be denied");

    // 3. DELETE query -> DENY
    let mut req_delete = EvaluationRequest {
        principal: "Agent::\"urn:vam:agent:sqlite-assistant\"".to_string(),
        action: "Action::\"ExecuteTool\"".to_string(),
        resource: "Tool::\"query_db\"".to_string(),
        context: EvaluationContext::default(),
        entities: Vec::new(),
    };
    req_delete.context.query = Some("DELETE FROM facts WHERE 1=1".to_string());

    let outcome_delete = engine.evaluate(&req_delete);
    assert!(outcome_delete.is_denied(), "DELETE query must be denied");
}

#[test]
fn test_write_file_size_cap() {
    let engine = CedarPolicyEngine::from_default_schema(WRITE_CAP_POLICY, PolicyMode::Enforce, 1)
        .expect("Policy compilation should succeed");

    // Under 10MB -> ALLOW
    let mut req_small = EvaluationRequest {
        principal: "Agent::\"urn:vam:agent:writer\"".to_string(),
        action: "Action::\"WriteFile\"".to_string(),
        resource: "Path::\"/mnt/drive9/workspace/test.txt\"".to_string(),
        context: EvaluationContext::default(),
        entities: Vec::new(),
    };
    req_small.context.file_size_bytes = Some(1024); // 1 KB
    assert!(engine.evaluate(&req_small).is_allowed());

    // Over 10MB -> DENY
    let mut req_large = EvaluationRequest {
        principal: "Agent::\"urn:vam:agent:writer\"".to_string(),
        action: "Action::\"WriteFile\"".to_string(),
        resource: "Path::\"/mnt/drive9/workspace/large.bin\"".to_string(),
        context: EvaluationContext::default(),
        entities: Vec::new(),
    };
    req_large.context.file_size_bytes = Some(15 * 1024 * 1024); // 15 MB
    assert!(engine.evaluate(&req_large).is_denied());
}

#[test]
fn test_deny_by_default() {
    let engine = CedarPolicyEngine::empty(PolicyMode::Enforce, 1)
        .expect("Empty policy set compilation should succeed");

    let req = EvaluationRequest {
        principal: "Agent::\"urn:vam:agent:unknown\"".to_string(),
        action: "Action::\"ExecuteTool\"".to_string(),
        resource: "Tool::\"probe_sandbox\"".to_string(),
        context: EvaluationContext::default(),
        entities: Vec::new(),
    };

    let outcome = engine.evaluate(&req);
    assert!(
        outcome.is_denied(),
        "Empty policy set must deny all requests by default"
    );
}

#[test]
fn playbook_context_compiles_and_denies_from_step_state() {
    let policy = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"jira_search_issues"
);
forbid (
    principal,
    action == Action::"ExecuteTool",
    resource
)
when {
    context has playbook &&
    context.playbook.contains({id: "approve", state: "pending", outcome: ""})
};
"#;
    let engine = CedarPolicyEngine::from_default_schema(policy, PolicyMode::Enforce, 1)
        .expect("context.playbook must compile against the default schema");

    let request = |playbook: serde_json::Value| {
        let mut req = EvaluationRequest {
            principal: "Agent::\"urn:vam:agent:playbook\"".to_string(),
            action: "Action::\"ExecuteTool\"".to_string(),
            resource: "Tool::\"jira_search_issues\"".to_string(),
            context: EvaluationContext::default(),
            entities: Vec::new(),
        };
        req.context
            .custom_attributes
            .insert("playbook".to_string(), playbook);
        req
    };

    let pending = engine.evaluate(&request(serde_json::json!([
        {"id": "approve", "state": "pending", "outcome": ""}
    ])));
    assert!(
        pending.is_denied(),
        "pending approve step must deny: {:?}",
        pending.diagnostic_message()
    );

    let approved = engine.evaluate(&request(serde_json::json!([
        {"id": "approve", "state": "completed", "outcome": "approved"}
    ])));
    assert!(
        approved.is_allowed(),
        "completed approve step must allow: {:?}",
        approved.diagnostic_message()
    );

    let mut absent = EvaluationRequest {
        principal: "Agent::\"urn:vam:agent:playbook\"".to_string(),
        action: "Action::\"ExecuteTool\"".to_string(),
        resource: "Tool::\"jira_search_issues\"".to_string(),
        context: EvaluationContext::default(),
        entities: Vec::new(),
    };
    absent.context.query = None;
    assert!(
        engine.evaluate(&absent).is_allowed(),
        "omitted playbook context stays on the permit path"
    );
}

#[test]
fn test_schema_validation_catches_invalid_attributes() {
    let bad_policy = r#"
    permit (
        principal == Agent::"test",
        action == Action::"ExecuteTool",
        resource == Tool::"test"
    )
    when {
        context.non_existent_attribute > 10
    };
    "#;

    let res = CedarPolicyEngine::from_default_schema(bad_policy, PolicyMode::Enforce, 1);
    assert!(
        res.is_err(),
        "Schema validation must reject undeclared context attributes"
    );
}

#[test]
fn test_generation_monotonicity_and_hot_reload() {
    let engine_v1 =
        CedarPolicyEngine::from_default_schema(SQL_POLICY, PolicyMode::Enforce, 1).unwrap();
    let arc_swap_engine = ArcSwapPolicyEngine::new(engine_v1);

    // Attempting reload with equal or lower generation should fail
    let engine_stale =
        CedarPolicyEngine::from_default_schema(SQL_POLICY, PolicyMode::Enforce, 1).unwrap();
    assert!(arc_swap_engine.hot_reload(engine_stale).is_err());

    // Higher generation succeeds
    let engine_v2 =
        CedarPolicyEngine::from_default_schema(SQL_POLICY, PolicyMode::Enforce, 2).unwrap();
    assert!(arc_swap_engine.hot_reload(engine_v2).is_ok());
    assert_eq!(arc_swap_engine.generation(), 2);
}

#[test]
fn test_shadow_mode_permits_with_logging() {
    let engine = CedarPolicyEngine::from_default_schema(SQL_POLICY, PolicyMode::Shadow, 1).unwrap();

    let mut req_drop = EvaluationRequest {
        principal: "Agent::\"urn:vam:agent:sqlite-assistant\"".to_string(),
        action: "Action::\"ExecuteTool\"".to_string(),
        resource: "Tool::\"query_db\"".to_string(),
        context: EvaluationContext::default(),
        entities: Vec::new(),
    };
    req_drop.context.query = Some("DROP TABLE facts".to_string());

    let outcome = engine.evaluate(&req_drop);
    assert!(outcome.is_allowed(), "Shadow mode must permit execution");
    if let DecisionOutcome::Allow { matching_policies } = outcome {
        assert!(matching_policies
            .iter()
            .any(|p| p.contains("SHADOW_PERMIT")));
    }
}

#[test]
fn test_capability_token_lifecycle() {
    let signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let signer = ApprovalSigner::new("host-test-key", signing_key);
    let verifying_key = signer.verifying_key();
    let replay_store = InMemoryReplayStore::default();

    let env_spec = vam_spec::v1::ResourceEnvelope {
        max_wall_time_ms: 30_000,
        max_cpu_time_ms: 10_000,
        max_memory_bytes: 512 * 1024 * 1024,
        max_procs: 32,
        max_open_files: 128,
        max_file_size_bytes: 10 * 1024 * 1024,
        max_egress_bytes: 1024 * 1024,
        max_output_bytes: 2 * 1024 * 1024,
    };

    let claims = CapabilityClaims {
        session_id: "sess-123".to_string(),
        lease_id: "lease-456".to_string(),
        lifecycle_generation: 7,
        tool_name: "query_db".to_string(),
        arguments_digest: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            .to_string(),
        script_digest: "a1b2c3d4e5f6".to_string(),
        call_id: String::new(),
        resource_envelope: Some(env_spec.clone()),
        expires_at: 1000,
        nonce: "nonce-xyz-1".to_string(),
    };

    let envelope = mint_capability_token(&signer, &claims).expect("Token minting should succeed");

    // 1. Successful verification
    let res = verify_capability_token(
        "host-test-key",
        &verifying_key,
        &envelope,
        "sess-123",
        "lease-456",
        7,
        "query_db",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "a1b2c3d4e5f6",
        Some(&env_spec),
        900, // current time < expires_at
        &replay_store,
    );
    assert!(res.is_ok(), "Valid token must verify");

    // 2. Replay attempt -> ReplayStore rejects
    let replay_res = verify_capability_token(
        "host-test-key",
        &verifying_key,
        &envelope,
        "sess-123",
        "lease-456",
        7,
        "query_db",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "a1b2c3d4e5f6",
        Some(&env_spec),
        900,
        &replay_store,
    );
    assert!(
        replay_res.is_err(),
        "Spent token must be rejected by ReplayStore"
    );

    // 3. Arguments swap (TOCTOU)
    let claims2 = CapabilityClaims {
        nonce: "nonce-xyz-2".to_string(),
        ..claims.clone()
    };
    let envelope2 = mint_capability_token(&signer, &claims2).unwrap();
    let swap_res = verify_capability_token(
        "host-test-key",
        &verifying_key,
        &envelope2,
        "sess-123",
        "lease-456",
        7,
        "query_db",
        "tampered_digest_value",
        "a1b2c3d4e5f6",
        Some(&env_spec),
        900,
        &replay_store,
    );
    assert!(
        swap_res.is_err(),
        "Arguments digest mismatch must fail verification"
    );

    // 4. Script digest swap (TOCTOU code tampering)
    let claims3 = CapabilityClaims {
        nonce: "nonce-xyz-3".to_string(),
        ..claims.clone()
    };
    let envelope3 = mint_capability_token(&signer, &claims3).unwrap();
    let script_swap_res = verify_capability_token(
        "host-test-key",
        &verifying_key,
        &envelope3,
        "sess-123",
        "lease-456",
        7,
        "query_db",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "tampered_script_digest",
        Some(&env_spec),
        900,
        &replay_store,
    );
    assert!(
        script_swap_res.is_err(),
        "Script digest mismatch must fail verification"
    );

    // 5. Envelope widening / tampering
    let widened_spec = vam_spec::v1::ResourceEnvelope {
        max_memory_bytes: 4 * 1024 * 1024 * 1024,
        ..env_spec.clone()
    };
    let envelope_tamper_res = verify_capability_token(
        "host-test-key",
        &verifying_key,
        &envelope3,
        "sess-123",
        "lease-456",
        7,
        "query_db",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "a1b2c3d4e5f6",
        Some(&widened_spec),
        900,
        &replay_store,
    );
    assert!(
        envelope_tamper_res.is_err(),
        "Envelope tampering/widening must fail verification"
    );
}

#[test]
fn test_audit_ledger_chain_and_forking() {
    let signing_key = SigningKey::from_bytes(&[99u8; 32]);
    let verifying_key = signing_key.verifying_key();
    let ledger = AuditLedger::new("test-audit", signing_key, None);

    let block1 = ledger
        .record_decision_with_telemetry(
            "sess-1",
            "Agent::\"bot\"",
            "Action::\"ExecuteTool\"",
            "Tool::\"echo\"",
            "ALLOW",
            vec!["policy0".to_string()],
            "",
            &serde_json::json!({"args": "hello"}),
            "bundle-digest-1",
            Some("script-sha256-111".to_string()),
            Some(120),              // 120 ms CPU
            Some(45 * 1024 * 1024), // 45 MB peak RSS
            Some(250),              // 250 ms wall time
            Some(1024),             // 1024 bytes egress
        )
        .expect("Recording block 1 should succeed");

    assert_eq!(block1.script_digest.as_deref(), Some("script-sha256-111"));
    assert_eq!(block1.cpu_time_ms, Some(120));
    assert_eq!(block1.peak_rss_bytes, Some(45 * 1024 * 1024));

    let block2 = ledger
        .record_decision(
            "sess-1",
            "Agent::\"bot\"",
            "Action::\"ExecuteTool\"",
            "Tool::\"cat\"",
            "DENY",
            vec![],
            "Forbidden",
            &serde_json::json!({"file": "/etc/shadow"}),
            "bundle-digest-1",
        )
        .expect("Recording block 2 should succeed");

    let chain = ledger.get_chain().unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(block2.previous_hash, block1.compute_block_hash());

    // Verify chain cryptographically
    AuditLedger::verify_chain(&chain, &verifying_key)
        .expect("Audit chain verification must succeed");

    // Fork at snapshot
    let forked_ledger = ledger
        .fork_at_snapshot("snap-ckpt-100".to_string())
        .unwrap();
    let forked_block1 = forked_ledger
        .record_decision(
            "sess-1-resumed",
            "Agent::\"bot\"",
            "Action::\"ExecuteTool\"",
            "Tool::\"echo\"",
            "ALLOW",
            vec![],
            "",
            &serde_json::json!({}),
            "bundle-digest-1",
        )
        .unwrap();

    let forked_chain = forked_ledger.get_chain().unwrap();
    assert_eq!(forked_chain.len(), 1);
    assert_eq!(
        forked_block1.parent_snapshot_id.as_deref(),
        Some("snap-ckpt-100")
    );
    AuditLedger::verify_chain(&forked_chain, &verifying_key)
        .expect("Forked audit chain must verify");
}
