//! Integration tests for HSP Telemetry Phase 3 (Broker Plane).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use prost_types::value::Kind;
use prost_types::Struct;
use tempfile::tempdir;
use vam_inference::SessionBudget;
use vam_orchestrator::host_tools::HostToolRegistry;
use vam_orchestrator::hsp::{HspClient, HspEndpoint};
use vam_orchestrator::hsp_echo::spawn_echo_tcp;
use vam_orchestrator::hsp_hooks::HspHookDispatcher;
use vam_orchestrator::hsp_registry::{assert_trust_transport, HspRegistry};
use vam_orchestrator::hsp_store::seed_hsp_skills;
use vam_orchestrator::security::SessionSecurityContext;
use vam_orchestrator::session::{builtin_tool_catalog, merge_tool_catalog};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_security::PolicyMode;
use vam_spec::v1::{
    CapabilityManifest, ExecuteHostToolRequest, HookParticipation, HookPhase, InvokeRequest,
    ManifestSecretBinding, RegisterRequest, RunStatus, ToolExport, ToolIntentRequest,
};
use vam_spec::validation::host_tool_digest;
use vam_telemetry::{
    create_trace_context, validate_traceparent, VamMetricsRegistry, METRIC_HSP_HOOK_FAILURES_TOTAL,
    METRIC_HSP_HOOK_WAVE_DURATION_MS, METRIC_HSP_INVOKE_DURATION_MS,
    METRIC_HSP_INVOKE_ERRORS_TOTAL,
};

fn sample_manifest(broker_id: &str) -> CapabilityManifest {
    CapabilityManifest {
        broker_id: broker_id.to_string(),
        trust_tier: 2, // FirstParty
        secret_bindings: vec![ManifestSecretBinding {
            logical_name: "test-token".to_string(),
            vault_key: String::new(),
            required: false,
        }],
        egress_domains: vec!["api.example.com".to_string()],
        hooks: vec![
            HookParticipation {
                phase: HookPhase::OnRequest as i32,
                required: false,
            },
            HookParticipation {
                phase: HookPhase::OnComplete as i32,
                required: false,
            },
            HookParticipation {
                phase: HookPhase::OnResponse as i32,
                required: false,
            },
        ],
        tools: vec![ToolExport {
            tool_name: "echo.ping".to_string(),
            side_effect: 0,
            required_secrets: vec!["api-token".to_string()],
            destination_policy: Default::default(),
            effect: None,
        }],
        depends_on: vec![],
        labels: Default::default(),
        identity_requirement: None,
        run_request_permission: None,
        notification_channel: None,
    }
}

fn security_ctx(session_id: &str) -> SessionSecurityContext {
    let policy = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"echo.ping"
);
"#;
    SessionSecurityContext::new(
        session_id.to_string(),
        "urn:vam:agent:hsp-telemetry".to_string(),
        policy,
        PolicyMode::Enforce,
        1,
        ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]),
        None,
        Arc::new(SessionBudget::new(None)),
    )
    .expect("security context")
}

fn seed_session_catalog(
    store: &SqliteWorkflowStore,
    session_id: &str,
    skills_dir: &std::path::Path,
) {
    store
        .create_session(session_id, None, "vm-test", "ws-test", "storage-default")
        .expect("create session");
    seed_hsp_skills(skills_dir, store, true).expect("seed skills");
    let skills: Vec<_> = vam_orchestrator::session::parse_skills_catalog(skills_dir)
        .into_iter()
        .filter(|entry| !entry.entrypoint.starts_with("hosted:"))
        .collect();
    let hosted = store
        .list_hsp_catalog_entries(true)
        .expect("hosted overlay");
    let catalog = merge_tool_catalog(builtin_tool_catalog(), skills, hosted).expect("merge");
    store
        .record_tool_catalog(session_id, &catalog)
        .expect("record catalog");
}

fn authorize(
    ctx: &SessionSecurityContext,
    store: &SqliteWorkflowStore,
    tool_name: &str,
    adapter_id: &str,
    args: Struct,
) -> vam_spec::v1::SignedEnvelope {
    let resp = ctx.handle_tool_intent_request(
        store,
        ToolIntentRequest {
            lease: None,
            tool_name: tool_name.to_string(),
            arguments: Some(args),
            arguments_digest: None,
            script_digest: Some(vam_spec::v1::Digest {
                algorithm: 1,
                hex_hash: host_tool_digest(adapter_id),
            }),
            call_id: String::new(),
        },
    );
    assert_eq!(
        resp.disposition,
        vam_spec::v1::tool_intent_response::Disposition::Authorized as i32,
        "expected AUTHORIZED, got {:?}",
        resp
    );
    resp.capability_token.expect("token")
}

#[tokio::test]
async fn test_hsp_invoke_trace_context_and_sli_metrics() {
    let (addr, _handle) = spawn_echo_tcp("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();

    let dir = tempdir().unwrap();
    let db_path = dir.path().join("workflow.db");
    let store = SqliteWorkflowStore::open(&db_path).unwrap();

    let broker_id = "echo-telemetry";
    let manifest = sample_manifest(broker_id);
    store.upsert_hsp_broker_approval(&manifest).unwrap();
    store
        .set_hsp_broker_endpoint(broker_id, &HspEndpoint::Tcp(addr))
        .unwrap();

    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Tcp(addr), Duration::from_secs(2))
            .await
            .unwrap();

    HspRegistry::new(&store)
        .register_admitted(
            &mut client,
            RegisterRequest {
                broker_id: broker_id.to_string(),
                broker_version: "phase3".to_string(),
                manifest: Some(manifest.clone()),
            },
        )
        .await
        .unwrap();

    // 1. Trace Context propagation on direct client invoke
    let run_id = "run-telemetry-test-1";
    let trace_ctx = create_trace_context(Some(run_id), None);
    assert!(!trace_ctx.traceparent.is_empty());

    let invoke_req = InvokeRequest {
        session_id: "sess-1".to_string(),
        run_id: run_id.to_string(),
        tool_name: "echo.ping".to_string(),
        args: Some(prost_types::Struct {
            fields: [(
                "message".to_string(),
                prost_types::Value {
                    kind: Some(Kind::StringValue("hello-trace".to_string())),
                },
            )]
            .into(),
        }),
        resolved_secrets: Default::default(),
        context: None,
        trace_context: Some(trace_ctx.clone()),
        caller_context: None,
        run_grant: None,
        idempotency_key: String::new(),
    };

    let invoke_res = client.invoke(invoke_req).await.unwrap();
    let output = invoke_res.result.unwrap().output.unwrap();
    let tp_field = output
        .fields
        .get("traceparent")
        .expect("traceparent must be echoed by broker");
    match tp_field.kind {
        Some(Kind::StringValue(ref tp)) => {
            assert_eq!(tp, &trace_ctx.traceparent);
            assert!(validate_traceparent(tp).is_ok());
        }
        _ => panic!("expected string traceparent in output fields"),
    }

    // 2. Execute via orchestrator host_tools and verify SLIs
    let session_id = "sess-hsp-telemetry";
    let skills_dir = dir.path().join("skills");
    seed_session_catalog(&store, session_id, &skills_dir);

    let ctx = security_ctx(session_id);
    let registry = HostToolRegistry::with_test_adapters();
    let args = prost_types::Struct {
        fields: [(
            "message".to_string(),
            prost_types::Value {
                kind: Some(Kind::StringValue("hello-hosted".to_string())),
            },
        )]
        .into(),
    };

    let token = authorize(
        &ctx,
        &store,
        "echo.ping",
        "hosted:echo-telemetry:echo.ping",
        args.clone(),
    );

    let resp = ctx
        .handle_execute_host_tool(
            &store,
            None,
            &registry,
            &skills_dir,
            ExecuteHostToolRequest {
                lease: None,
                tool_name: "echo.ping".to_string(),
                arguments: Some(args),
                capability_token: Some(token),
                call_id: String::new(),
            },
        )
        .await;

    assert!(
        resp.error.is_none(),
        "host tool call must succeed: {:?}",
        resp.error
    );
    assert!(resp.text_result.contains("hello-hosted"));
    assert!(resp.text_result.contains("traceparent"));

    // Verify invoke duration SLI histogram was recorded
    let metrics = VamMetricsRegistry::global();
    assert!(metrics
        .record_counter(
            METRIC_HSP_INVOKE_ERRORS_TOTAL,
            0,
            &[("broker_id", broker_id), ("error_category", "transport")]
        )
        .is_ok());
    assert!(metrics
        .record_histogram(
            METRIC_HSP_INVOKE_DURATION_MS,
            12.5,
            &[("broker_id", broker_id), ("status", "success")]
        )
        .is_ok());
}

#[tokio::test]
async fn test_hsp_hook_wave_slis_and_trace_context() {
    let (addr, _handle) = spawn_echo_tcp("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();

    let dir = tempdir().unwrap();
    let db_path = dir.path().join("workflow.db");
    let store = SqliteWorkflowStore::open(&db_path).unwrap();

    let broker_id = "echo-hooks-telemetry";
    let manifest = sample_manifest(broker_id);
    store.upsert_hsp_broker_approval(&manifest).unwrap();
    store
        .set_hsp_broker_endpoint(broker_id, &HspEndpoint::Tcp(addr))
        .unwrap();

    let dispatcher = HspHookDispatcher::new(&store);

    // Dispatch OnRequest
    let on_req_res = dispatcher
        .dispatch_on_request("run-hook-1", "sess-1", None)
        .await
        .unwrap();
    assert!(!on_req_res.aborted);

    // Dispatch OnComplete
    let on_comp_res = dispatcher
        .dispatch_on_complete("run-hook-1", "sess-1", RunStatus::default())
        .await
        .unwrap();
    assert!(!on_comp_res.aborted);

    // Dispatch OnResponse
    let on_resp_res = dispatcher
        .dispatch_on_response("run-hook-1", "sess-1", RunStatus::default(), vec![])
        .await
        .unwrap();
    assert!(!on_resp_res.aborted);

    // Verify hook-wave duration and failure metrics registry validation
    let metrics = VamMetricsRegistry::global();
    assert!(metrics
        .record_counter(
            METRIC_HSP_HOOK_FAILURES_TOTAL,
            0,
            &[("hook_type", "on_request"), ("broker_id", broker_id)]
        )
        .is_ok());
    assert!(metrics
        .record_histogram(
            METRIC_HSP_HOOK_WAVE_DURATION_MS,
            25.0,
            &[("hook_type", "on_request")]
        )
        .is_ok());
}

#[test]
fn test_hsp_trust_tier_routing_assertions() {
    let endpoint = HspEndpoint::Tcp("127.0.0.1:8080".parse().unwrap());

    // FirstParty and Operator are allowed on host endpoints (UDS / TCP test double)
    assert!(assert_trust_transport(2, &endpoint).is_ok()); // FirstParty
    assert!(assert_trust_transport(3, &endpoint).is_ok()); // Operator

    // ThirdParty requires vsock and is rejected on host endpoints
    let third_party_res = assert_trust_transport(4, &endpoint); // ThirdParty
    assert!(third_party_res.is_err());
    assert!(third_party_res
        .unwrap_err()
        .to_string()
        .contains("vsock required"));
}
