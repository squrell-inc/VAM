use ed25519_dalek::SigningKey;
use prost_types::Struct;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use vam_inference::SessionBudget;
use vam_orchestrator::security::SessionSecurityContext;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_runtime::pep::verify_guest_capability_token;
use vam_security::{
    mint_capability_token, verify_capability_token, ApprovalSigner, CapabilityClaims,
    InMemoryReplayStore, PolicyMode,
};
use vam_spec::v1::{
    tool_intent_response::Disposition, Cost, Digest, ExecutionLease, ResourceId, ToolIntentRequest,
};

fn setup_test_host_and_store() -> (Arc<SqliteWorkflowStore>, SessionSecurityContext, SigningKey) {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let session_id = "test-session-zero-trust".to_string();
    assert!(store
        .create_session(
            &session_id,
            None,
            "vm-test",
            "sqlite-assistant",
            "storage-default"
        )
        .is_ok());
    let agent_urn = "urn:vam:agent:local.vam.dev/vam/sqlite-assistant".to_string();
    let policy_src = include_str!("../../../examples/sqlite-assistant/policy.cedar");
    let host_signing_key = SigningKey::from_bytes(&[55u8; 32]);
    let budget = Arc::new(SessionBudget::new(Some(Cost {
        currency: "USD".to_string(),
        micro_amount: 1_000_000,
    })));

    let sec_ctx = SessionSecurityContext::new(
        session_id.clone(),
        agent_urn,
        policy_src,
        PolicyMode::Enforce,
        1,
        host_signing_key.clone(),
        None,
        budget,
    )
    .unwrap();

    // Register valid tools in host authoritative catalog
    let catalog = vec![
        vam_orchestrator::ToolCatalogEntry {
            tool_name: "query_db".to_string(),
            command: "python".to_string(),
            entrypoint: "query.py".to_string(),
            digest: "digest1".to_string(),
            side_effect: 1, // SideEffect::Read
            skill_name: String::new(),
            approval_required: false,
        },
        vam_orchestrator::ToolCatalogEntry {
            tool_name: "insert_fact".to_string(),
            command: "python".to_string(),
            entrypoint: "insert.py".to_string(),
            digest: "digest2".to_string(),
            side_effect: 2, // SideEffect::MutableInternal
            skill_name: String::new(),
            approval_required: false,
        },
        vam_orchestrator::ToolCatalogEntry {
            tool_name: "init_db".to_string(),
            command: "python".to_string(),
            entrypoint: "init.py".to_string(),
            digest: "digest3".to_string(),
            side_effect: 2,
            skill_name: String::new(),
            approval_required: false,
        },
    ];
    store.record_tool_catalog(&session_id, &catalog).unwrap();

    (store, sec_ctx, host_signing_key)
}

/// Guest call site: `pep.rs` and `exec.rs` both call this function.
fn guest_args_digest(args: &Struct) -> String {
    vam_spec::canonical_args_digest(Some(args)).expect("canonical args digest")
}

fn create_lease(session_id: &str, generation: i64) -> ExecutionLease {
    ExecutionLease {
        run_id: Some(ResourceId {
            urn: "urn:vam:run:1".to_string(),
        }),
        step_id: "step-1".to_string(),
        attempt_id: Some(ResourceId {
            urn: "urn:vam:attempt:1".to_string(),
        }),
        lease_id: Some(ResourceId {
            urn: format!("urn:vam:lease:{}", session_id),
        }),
        lifecycle_generation: generation,
        fencing_token: vec![1, 2, 3],
        expires_at: Some(prost_types::Timestamp {
            seconds: 4_102_444_800,
            nanos: 0,
        }),
        idempotency_key: "idem-1".to_string(),
    }
}

fn json_string(s: &str) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(s.to_string())),
    }
}

fn json_number(n: f64) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::NumberValue(n)),
    }
}

fn json_null() -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::NullValue(0)),
    }
}

fn json_struct(fields: Vec<(&str, prost_types::Value)>) -> prost_types::Value {
    let mut inner = Struct::default();
    for (key, value) in fields {
        inner.fields.insert(key.to_string(), value);
    }
    prost_types::Value {
        kind: Some(prost_types::value::Kind::StructValue(inner)),
    }
}

fn json_list(values: Vec<prost_types::Value>) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::ListValue(
            prost_types::ListValue { values },
        )),
    }
}

/// Host mints a token from these arguments; the guest digest must verify it.
#[test]
fn canonical_args_digest_round_trips_structured_values() {
    let (store, sec_ctx, host_key) = setup_test_host_and_store();
    let host_vk = host_key.verifying_key();
    let cases = [
        (
            "array",
            json_list(vec![
                json_string("a"),
                json_number(1.0),
                json_struct(vec![("id", json_number(7.0))]),
            ]),
        ),
        (
            "nested",
            json_struct(vec![("inner", json_struct(vec![("n", json_number(3.0))]))]),
        ),
        ("missing", json_null()),
        ("empty", json_struct(vec![])),
    ];

    for (name, value) in cases {
        let mut args = Struct::default();
        args.fields
            .insert("query".to_string(), json_string("SELECT 1"));
        args.fields.insert(name.to_string(), value);

        let resp = sec_ctx.handle_tool_intent_request(
            &store,
            ToolIntentRequest {
                lease: Some(create_lease("test-session-zero-trust", 1)),
                tool_name: "query_db".to_string(),
                arguments: Some(args.clone()),
                arguments_digest: None,
                script_digest: Some(Digest {
                    algorithm: 1,
                    hex_hash: "digest1".to_string(),
                }),
                call_id: String::new(),
            },
        );
        assert_eq!(
            resp.disposition,
            Disposition::Authorized as i32,
            "{name} authorization failed: {:?}",
            resp.error
        );
        let token = resp
            .capability_token
            .unwrap_or_else(|| panic!("{name} token missing"));
        let guest_digest = guest_args_digest(&args);
        let claims = vam_security::parse_capability_claims(&token).expect("claims");
        assert_eq!(
            claims.arguments_digest, guest_digest,
            "{name} host digest diverged from the guest call site"
        );

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        verify_capability_token(
            "host-key-test-session-zero-trust",
            &host_vk,
            &token,
            "test-session-zero-trust",
            "urn:vam:lease:test-session-zero-trust",
            1,
            "query_db",
            &guest_digest,
            "digest1",
            None,
            now,
            &InMemoryReplayStore::default(),
        )
        .unwrap_or_else(|e| panic!("{name} guest verification failed: {e}"));
    }
}

// 1. attack_guest_bypass_tool_authorization
#[test]
fn test_attack_1_guest_bypass_tool_authorization() {
    let (_, _, host_key) = setup_test_host_and_store();
    let host_vk = host_key.verifying_key();

    // Guest tries to verify without a token (or with empty envelope)
    let bad_envelope = vam_spec::v1::SignedEnvelope {
        domain: "vam.v1.tool-capability".to_string(),
        envelope_version: None,
        claims: vec![],
        signature: None,
    };

    let res = verify_guest_capability_token(
        &bad_envelope,
        Some(&host_vk),
        "query_db",
        "digest123",
        "digest1",
        None,
        Some("urn:vam:lease:test-session-zero-trust"),
    );
    assert!(
        res.is_err(),
        "Bypassed tool invocation without valid token must be rejected"
    );
}

// 2. attack_forged_capability_token
#[test]
fn test_attack_2_forged_capability_token() {
    let (_, _, host_key) = setup_test_host_and_store();
    let host_vk = host_key.verifying_key();

    // Malicious guest mints token using its own key
    let rogue_key = SigningKey::from_bytes(&[99u8; 32]);
    let rogue_signer = ApprovalSigner::new("host-key-test-session-zero-trust", rogue_key);

    let claims = CapabilityClaims {
        session_id: "test-session-zero-trust".to_string(),
        lease_id: "urn:vam:lease:test-session-zero-trust".to_string(),
        lifecycle_generation: 1,
        tool_name: "query_db".to_string(),
        arguments_digest: "digest123".to_string(),
        script_digest: "digest1".to_string(),
        call_id: String::new(),
        resource_envelope: None,
        expires_at: 4_000_000_000,
        nonce: "rogue-nonce-1".to_string(),
    };

    let forged_token = mint_capability_token(&rogue_signer, &claims).unwrap();

    let res = verify_guest_capability_token(
        &forged_token,
        Some(&host_vk),
        "query_db",
        "digest123",
        "digest1",
        None,
        Some("urn:vam:lease:test-session-zero-trust"),
    );
    assert!(
        res.is_err(),
        "Forged token signed by rogue key must be rejected by host VK"
    );
}

// 3. attack_expired_capability_token
#[test]
fn test_attack_3_expired_capability_token() {
    let (store, sec_ctx, host_key) = setup_test_host_and_store();
    let host_vk = host_key.verifying_key();

    let mut args = Struct::default();
    args.fields.insert(
        "query".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "SELECT 1".to_string(),
            )),
        },
    );

    let lease = create_lease("test-session-zero-trust", 1);
    let req = ToolIntentRequest {
        lease: Some(lease),
        tool_name: "query_db".to_string(),
        arguments: Some(args.clone()),
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "digest1".to_string(),
        }),
        call_id: String::new(),
    };

    let resp = sec_ctx.handle_tool_intent_request(&store, req);
    assert_eq!(resp.disposition, Disposition::Authorized as i32);
    let token = resp.capability_token.expect("Must have capability token");

    let computed_digest = guest_args_digest(&args);

    // Fast-forward time past 30s TTL by checking with expired current_time
    let replay_store = InMemoryReplayStore::default();
    let expired_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 100;

    let res = verify_capability_token(
        "host-key-test-session-zero-trust",
        &host_vk,
        &token,
        "test-session-zero-trust",
        "urn:vam:lease:test-session-zero-trust",
        1,
        "query_db",
        &computed_digest,
        "digest1",
        None,
        expired_time,
        &replay_store,
    );
    assert!(
        res.is_err(),
        "Token past 30s TTL expiration must be rejected"
    );
}

// 4. attack_capability_token_replay
#[test]
fn test_attack_4_capability_token_replay() {
    let (store, sec_ctx, host_key) = setup_test_host_and_store();
    let host_vk = host_key.verifying_key();

    let mut args = Struct::default();
    args.fields.insert(
        "query".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "SELECT count(*) FROM facts".to_string(),
            )),
        },
    );

    let lease = create_lease("test-session-zero-trust", 1);
    let req = ToolIntentRequest {
        lease: Some(lease),
        tool_name: "query_db".to_string(),
        arguments: Some(args.clone()),
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "digest1".to_string(),
        }),
        call_id: String::new(),
    };

    let resp = sec_ctx.handle_tool_intent_request(&store, req);
    let token = resp.capability_token.unwrap();
    let computed_digest = guest_args_digest(&args);

    let replay_store = InMemoryReplayStore::default();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // First use passes
    let res1 = verify_capability_token(
        "host-key-test-session-zero-trust",
        &host_vk,
        &token,
        "test-session-zero-trust",
        "urn:vam:lease:test-session-zero-trust",
        1,
        "query_db",
        &computed_digest,
        "digest1",
        None,
        now,
        &replay_store,
    );
    assert!(
        res1.is_ok(),
        "First consumption of capability token must succeed"
    );

    // Second use is replay attack -> rejected!
    let res2 = verify_capability_token(
        "host-key-test-session-zero-trust",
        &host_vk,
        &token,
        "test-session-zero-trust",
        "urn:vam:lease:test-session-zero-trust",
        1,
        "query_db",
        &computed_digest,
        "digest1",
        None,
        now,
        &replay_store,
    );
    assert!(
        res2.is_err(),
        "Replayed capability token must be rejected by ReplayStore"
    );
}

// 5. attack_arguments_tampering_toctou
#[test]
fn test_attack_5_arguments_tampering_toctou() {
    let (store, sec_ctx, host_key) = setup_test_host_and_store();
    let host_vk = host_key.verifying_key();

    // Request authorization for SELECT (innocent)
    let mut args_select = Struct::default();
    args_select.fields.insert(
        "query".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "SELECT 1".to_string(),
            )),
        },
    );

    let lease = create_lease("test-session-zero-trust", 1);
    let req = ToolIntentRequest {
        lease: Some(lease),
        tool_name: "query_db".to_string(),
        arguments: Some(args_select.clone()),
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "digest1".to_string(),
        }),
        call_id: String::new(),
    };

    let resp = sec_ctx.handle_tool_intent_request(&store, req);
    let token = resp.capability_token.unwrap();

    // Guest tries to execute DROP query using the SELECT token
    let mut args_drop = Struct::default();
    args_drop.fields.insert(
        "query".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "DROP TABLE facts".to_string(),
            )),
        },
    );
    let drop_digest = guest_args_digest(&args_drop);

    let replay_store = InMemoryReplayStore::default();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let res = verify_capability_token(
        "host-key-test-session-zero-trust",
        &host_vk,
        &token,
        "test-session-zero-trust",
        "urn:vam:lease:test-session-zero-trust",
        1,
        "query_db",
        &drop_digest,
        "digest1",
        None,
        now,
        &replay_store,
    );
    assert!(
        res.is_err(),
        "Arguments tampering (TOCTOU swap) must fail arguments digest binding"
    );
}

// 6. attack_cross_session_capability_swap
#[test]
fn test_attack_6_cross_session_capability_swap() {
    let (store, sec_ctx_a, host_key) = setup_test_host_and_store();
    let host_vk = host_key.verifying_key();

    let mut args = Struct::default();
    args.fields.insert(
        "query".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "SELECT * FROM facts".to_string(),
            )),
        },
    );

    let lease = create_lease("test-session-zero-trust", 1);
    let req = ToolIntentRequest {
        lease: Some(lease),
        tool_name: "query_db".to_string(),
        arguments: Some(args.clone()),
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "digest1".to_string(),
        }),
        call_id: String::new(),
    };

    let resp = sec_ctx_a.handle_tool_intent_request(&store, req);
    let token = resp.capability_token.unwrap();
    let computed_digest = guest_args_digest(&args);

    let replay_store = InMemoryReplayStore::default();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Present token in Session B
    let res = verify_capability_token(
        "host-key-test-session-zero-trust",
        &host_vk,
        &token,
        "session-b-attacker", // Session B
        "urn:vam:lease:session-b-attacker",
        1,
        "query_db",
        &computed_digest,
        "digest1",
        None,
        now,
        &replay_store,
    );
    assert!(
        res.is_err(),
        "Cross-session capability token swap must be rejected"
    );
}

// 7. attack_cross_generation_capability_reuse
#[test]
fn test_attack_7_cross_generation_capability_reuse() {
    let (store, sec_ctx, host_key) = setup_test_host_and_store();
    let host_vk = host_key.verifying_key();

    let mut args = Struct::default();
    args.fields.insert(
        "query".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "SELECT 1".to_string(),
            )),
        },
    );

    // Generation 1 token
    let lease_gen1 = create_lease("test-session-zero-trust", 1);
    let req = ToolIntentRequest {
        lease: Some(lease_gen1),
        tool_name: "query_db".to_string(),
        arguments: Some(args.clone()),
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "digest1".to_string(),
        }),
        call_id: String::new(),
    };

    let resp = sec_ctx.handle_tool_intent_request(&store, req);
    let token_gen1 = resp.capability_token.unwrap();
    let computed_digest = guest_args_digest(&args);

    let replay_store = InMemoryReplayStore::default();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Present Generation 1 token in Generation 2 (e.g. after snapshot restore / lease fence)
    let res = verify_capability_token(
        "host-key-test-session-zero-trust",
        &host_vk,
        &token_gen1,
        "test-session-zero-trust",
        "urn:vam:lease:test-session-zero-trust",
        2, // Generation 2
        "query_db",
        &computed_digest,
        "digest1",
        None,
        now,
        &replay_store,
    );
    assert!(
        res.is_err(),
        "Stale generation token reuse must be rejected by generation fence"
    );
}

// 8. attack_unauthorized_tool_in_catalog
#[test]
fn test_attack_8_unauthorized_tool_in_catalog() {
    let (store, sec_ctx, _) = setup_test_host_and_store();

    // Guest attempts to authorize a tool created in guest filesystem but NOT in host catalog
    let mut args = Struct::default();
    args.fields.insert(
        "cmd".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "curl evil.com".to_string(),
            )),
        },
    );

    let lease = create_lease("test-session-zero-trust", 1);
    let req = ToolIntentRequest {
        lease: Some(lease),
        tool_name: "evil_injected_tool".to_string(),
        arguments: Some(args),
        arguments_digest: None,
        script_digest: None,
        call_id: String::new(),
    };

    let resp = sec_ctx.handle_tool_intent_request(&store, req);
    assert_eq!(resp.disposition, Disposition::Denied as i32);
    assert!(
        resp.error
            .unwrap()
            .message
            .contains("not present in the host-authoritative tool catalog"),
        "Uncataloged tool must be rejected immediately by host catalog gate"
    );
}

// 9. attack_egress_ssrf_metadata
#[test]
fn test_attack_9_egress_ssrf_metadata() {
    let metadata_ip: std::net::IpAddr = "169.254.169.254".parse().unwrap();
    let rfc1918_ip1: std::net::IpAddr = "10.0.0.1".parse().unwrap();
    let rfc1918_ip2: std::net::IpAddr = "192.168.1.1".parse().unwrap();
    let loopback_ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();

    assert!(
        vam_network::is_ip_denied(&metadata_ip),
        "Metadata IP 169.254.169.254 must be in deny set"
    );
    assert!(
        vam_network::is_ip_denied(&rfc1918_ip1),
        "RFC1918 10.0.0.1 must be in deny set"
    );
    assert!(
        vam_network::is_ip_denied(&rfc1918_ip2),
        "RFC1918 192.168.1.1 must be in deny set"
    );
    assert!(
        vam_network::is_ip_denied(&loopback_ip),
        "Loopback 127.0.0.1 must be in deny set"
    );

    let public_ip: std::net::IpAddr = "93.184.216.34".parse().unwrap(); // example.com
    assert!(
        !vam_network::is_ip_denied(&public_ip),
        "Public IP must not be denied by SSRF filter"
    );
}

// 10. attack_script_digest_catalog_tamper
#[test]
fn test_attack_10_script_digest_catalog_tamper() {
    let (store, sec_ctx, _) = setup_test_host_and_store();

    let mut args = Struct::default();
    args.fields.insert(
        "query".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "SELECT 1".to_string(),
            )),
        },
    );

    let lease = create_lease("test-session-zero-trust", 1);
    // query_db expected digest in catalog is "digest1", but attacker provides "tampered_digest_999"
    let req = ToolIntentRequest {
        lease: Some(lease),
        tool_name: "query_db".to_string(),
        arguments: Some(args),
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "tampered_digest_999".to_string(),
        }),
        call_id: String::new(),
    };

    let resp = sec_ctx.handle_tool_intent_request(&store, req);
    assert_eq!(resp.disposition, Disposition::Denied as i32);
    let err = resp.error.expect("Must have error status");
    assert_eq!(err.code, 7); // PERMISSION_DENIED
    assert!(
        err.message.contains("Skill tamper detected"),
        "Tampered skill entrypoint must be denied: {}",
        err.message
    );
}

// 11. attack_script_digest_capability_token_mismatch
#[test]
fn test_attack_11_script_digest_capability_token_mismatch() {
    let (_, _, host_key) = setup_test_host_and_store();
    let host_vk = host_key.verifying_key();
    let signer = ApprovalSigner::new("host-key-test-session-zero-trust", host_key);

    let claims = CapabilityClaims {
        session_id: "test-session-zero-trust".to_string(),
        lease_id: "urn:vam:lease:test-session-zero-trust".to_string(),
        lifecycle_generation: 1,
        tool_name: "query_db".to_string(),
        arguments_digest: "args_digest_123".to_string(),
        script_digest: "authorized_script_digest_aaa".to_string(),
        call_id: String::new(),
        resource_envelope: None,
        expires_at: 4_000_000_000,
        nonce: "nonce-script-1".to_string(),
    };

    let token = mint_capability_token(&signer, &claims).unwrap();

    // Guest tries to verify token with a different executed script digest
    let res = verify_guest_capability_token(
        &token,
        Some(&host_vk),
        "query_db",
        "args_digest_123",
        "different_executed_script_digest_bbb",
        None,
        Some("urn:vam:lease:test-session-zero-trust"),
    );
    assert!(
        res.is_err(),
        "Capability token script digest mismatch must fail guest verification"
    );
}

// 12. dynamic_script_content_addressed_in_ledger
#[test]
fn test_attack_12_dynamic_script_content_addressed_in_ledger() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let session_id = "test-session-dynamic-script".to_string();
    assert!(store
        .create_session(
            &session_id,
            None,
            "vm-test",
            "python-agent",
            "storage-default"
        )
        .is_ok());
    let agent_urn = "urn:vam:agent:python-agent".to_string();
    let policy_src = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"run_python"
);
"#;
    let host_signing_key = SigningKey::from_bytes(&[77u8; 32]);
    let budget = Arc::new(SessionBudget::new(None));

    let sec_ctx = SessionSecurityContext::new(
        session_id.clone(),
        agent_urn,
        policy_src,
        PolicyMode::Enforce,
        1,
        host_signing_key,
        None,
        budget,
    )
    .unwrap();

    // Register a dynamic tool with empty catalog digest (e.g. dynamic agent script runner)
    let catalog = vec![vam_orchestrator::ToolCatalogEntry {
        tool_name: "run_python".to_string(),
        command: "python3".to_string(),
        entrypoint: "/work/dynamic_task.py".to_string(),
        digest: String::new(), // dynamic script authored by agent
        side_effect: 2,
        skill_name: String::new(),
        approval_required: false,
    }];
    store.record_tool_catalog(&session_id, &catalog).unwrap();

    let dynamic_script_hash = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    let mut args = Struct::default();
    args.fields.insert(
        "file".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "/work/dynamic_task.py".to_string(),
            )),
        },
    );

    let lease = create_lease("test-session-dynamic-script", 1);
    let req = ToolIntentRequest {
        lease: Some(lease),
        tool_name: "run_python".to_string(),
        arguments: Some(args),
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: dynamic_script_hash.to_string(),
        }),
        call_id: String::new(),
    };

    let resp = sec_ctx.handle_tool_intent_request(&store, req);
    assert_eq!(resp.disposition, Disposition::Authorized as i32);
    let token = resp
        .capability_token
        .expect("Must mint capability token for dynamic script");

    let claims = vam_security::parse_capability_claims(&token).expect("Must parse claims");
    assert_eq!(
        claims.script_digest, dynamic_script_hash,
        "Capability token must bind dynamic script digest"
    );
}

// 13. inline_shell_content_addressed
#[test]
fn test_attack_13_inline_shell_content_addressed() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let session_id = "test-session-shell".to_string();
    assert!(store
        .create_session(
            &session_id,
            None,
            "vm-test",
            "shell-agent",
            "storage-default"
        )
        .is_ok());
    let agent_urn = "urn:vam:agent:shell-agent".to_string();
    let policy_src = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"sh"
);
"#;
    let host_signing_key = SigningKey::from_bytes(&[88u8; 32]);
    let budget = Arc::new(SessionBudget::new(None));

    let sec_ctx = SessionSecurityContext::new(
        session_id.clone(),
        agent_urn,
        policy_src,
        PolicyMode::Enforce,
        1,
        host_signing_key,
        None,
        budget,
    )
    .unwrap();

    // Register shell tool with empty digest
    let catalog = vec![vam_orchestrator::ToolCatalogEntry {
        tool_name: "sh".to_string(),
        command: "/bin/sh".to_string(),
        entrypoint: String::new(),
        digest: String::new(),
        side_effect: 2,
        skill_name: String::new(),
        approval_required: false,
    }];
    store.record_tool_catalog(&session_id, &catalog).unwrap();

    let cmd_script = "echo 'running audit test'";
    use sha2::{Digest as ShaDigest, Sha256};
    let expected_cmd_hash = format!("{:x}", Sha256::digest(cmd_script.as_bytes()));

    let mut args = Struct::default();
    args.fields.insert(
        "command".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                cmd_script.to_string(),
            )),
        },
    );

    let lease = create_lease(&session_id, 1);
    let req = ToolIntentRequest {
        lease: Some(lease),
        tool_name: "sh".to_string(),
        arguments: Some(args),
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: expected_cmd_hash.clone(),
        }),
        call_id: String::new(),
    };

    let resp = sec_ctx.handle_tool_intent_request(&store, req);
    assert_eq!(resp.disposition, Disposition::Authorized as i32);
    let token = resp.capability_token.expect("Must mint capability token");
    let claims = vam_security::parse_capability_claims(&token).unwrap();
    assert_eq!(claims.script_digest, expected_cmd_hash);
}

// 14. attack_s2_resource_envelope_widening_tampering_rejected
#[test]
fn test_attack_14_s2_resource_envelope_widening_tampering_rejected() {
    let (_, _, host_key) = setup_test_host_and_store();
    let host_vk = host_key.verifying_key();
    let signer = ApprovalSigner::new("host-key-test-session-zero-trust", host_key);

    let issued_envelope = vam_spec::v1::ResourceEnvelope {
        max_wall_time_ms: 10_000,
        max_cpu_time_ms: 2_000,
        max_memory_bytes: 256 * 1024 * 1024,
        max_procs: 16,
        max_open_files: 64,
        max_file_size_bytes: 10 * 1024 * 1024,
        max_egress_bytes: 5 * 1024 * 1024,
        max_output_bytes: 1024 * 1024,
    };

    let claims = CapabilityClaims {
        session_id: "test-session-zero-trust".to_string(),
        lease_id: "urn:vam:lease:test-session-zero-trust".to_string(),
        lifecycle_generation: 1,
        tool_name: "query_db".to_string(),
        arguments_digest: "args_digest_123".to_string(),
        script_digest: "digest1".to_string(),
        call_id: String::new(),
        resource_envelope: Some(issued_envelope),
        expires_at: 4_000_000_000,
        nonce: "nonce-envelope-1".to_string(),
    };

    let token = mint_capability_token(&signer, &claims).unwrap();

    // Malicious guest tries to pass widened envelope (e.g. 100GB memory, 1000 procs)
    let widened_envelope = vam_spec::v1::ResourceEnvelope {
        max_wall_time_ms: 600_000,
        max_cpu_time_ms: 300_000,
        max_memory_bytes: 100 * 1024 * 1024 * 1024,
        max_procs: 1000,
        max_open_files: 5000,
        max_file_size_bytes: 10 * 1024 * 1024 * 1024,
        max_egress_bytes: 10 * 1024 * 1024 * 1024,
        max_output_bytes: 100 * 1024 * 1024,
    };

    let res = verify_guest_capability_token(
        &token,
        Some(&host_vk),
        "query_db",
        "args_digest_123",
        "digest1",
        Some(&widened_envelope),
        Some("urn:vam:lease:test-session-zero-trust"),
    );
    assert!(
        res.is_err(),
        "Guest verification must reject resource envelope mismatch/widening attempt"
    );
}

// 15. attack_s2_runaway_loop_iteration_metering_denied
#[test]
fn test_attack_15_s2_runaway_loop_iteration_metering_denied() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let session_id = "test-session-loop".to_string();
    assert!(store
        .create_session(
            &session_id,
            None,
            "vm-test",
            "loop-agent",
            "storage-default"
        )
        .is_ok());
    let agent_urn = "urn:vam:agent:loop-agent".to_string();

    // Cedar policy allowing up to 3 tool calls, then denying iteration > 3
    let policy_src = r#"
        permit(
            principal == Agent::"urn:vam:agent:loop-agent",
            action == Action::"ExecuteTool",
            resource
        ) when {
            context has iteration && context.iteration <= 3
        };
    "#;

    let host_signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let budget = Arc::new(SessionBudget::new(None));

    let sec_ctx = SessionSecurityContext::new(
        session_id.clone(),
        agent_urn,
        policy_src,
        PolicyMode::Enforce,
        1,
        host_signing_key,
        None,
        budget,
    )
    .unwrap();

    let catalog = vec![vam_orchestrator::ToolCatalogEntry {
        tool_name: "step_tool".to_string(),
        command: "python3".to_string(),
        entrypoint: "step.py".to_string(),
        digest: "digest_step".to_string(),
        side_effect: 1,
        skill_name: String::new(),
        approval_required: false,
    }];
    store.record_tool_catalog(&session_id, &catalog).unwrap();

    for iter in 1..=3 {
        let lease = create_lease(&session_id, 1);
        let req = ToolIntentRequest {
            lease: Some(lease),
            tool_name: "step_tool".to_string(),
            arguments: None,
            arguments_digest: None,
            script_digest: Some(Digest {
                algorithm: 1,
                hex_hash: "digest_step".to_string(),
            }),
            call_id: String::new(),
        };
        let resp = sec_ctx.handle_tool_intent_request(&store, req);
        assert_eq!(
            resp.disposition,
            Disposition::Authorized as i32,
            "Iteration {} must be permitted",
            iter
        );
    }

    // 4th iteration exceeds context.iteration <= 3 and is DENIED!
    let lease = create_lease(&session_id, 1);
    let req = ToolIntentRequest {
        lease: Some(lease),
        tool_name: "step_tool".to_string(),
        arguments: None,
        arguments_digest: None,
        script_digest: Some(Digest {
            algorithm: 1,
            hex_hash: "digest_step".to_string(),
        }),
        call_id: String::new(),
    };
    let resp = sec_ctx.handle_tool_intent_request(&store, req);
    assert_eq!(
        resp.disposition,
        Disposition::Denied as i32,
        "Iteration 4 must be denied by runaway loop meter"
    );
}

// 16. test_s2_session_resource_usage_baseline_storage
#[test]
fn test_s2_session_resource_usage_baseline_storage() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let session_id = "test-session-baseline";
    assert!(store
        .create_session(session_id, None, "vm-1", "ws-1", "prof-1")
        .is_ok());

    let script_digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    let usage1 = vam_spec::v1::ResourceUsage {
        wall_time_ms: 120,
        cpu_time_ms: 40,
        peak_rss_bytes: 10 * 1024 * 1024,
        bytes_written: 1024,
        exit_signal: 0,
        limit_exceeded: false,
        limit_type: String::new(),
    };

    let usage2 = vam_spec::v1::ResourceUsage {
        wall_time_ms: 160,
        cpu_time_ms: 60,
        peak_rss_bytes: 20 * 1024 * 1024,
        bytes_written: 2048,
        exit_signal: 0,
        limit_exceeded: false,
        limit_type: String::new(),
    };

    store
        .record_resource_usage(session_id, script_digest, "test_tool", &usage1)
        .unwrap();
    store
        .record_resource_usage(session_id, script_digest, "test_tool", &usage2)
        .unwrap();

    let baseline = store
        .get_digest_resource_baseline(script_digest)
        .unwrap()
        .expect("Must find baseline");
    // (avg_cpu, avg_rss, avg_wall, count)
    assert_eq!(baseline.0, 50); // (40 + 60) / 2
    assert_eq!(baseline.1, 15 * 1024 * 1024); // (10MB + 20MB) / 2
    assert_eq!(baseline.2, 140); // (120 + 160) / 2
    assert_eq!(baseline.3, 2); // 2 samples
}
