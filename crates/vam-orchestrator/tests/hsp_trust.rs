//! Integration tests for HSP Phase 6 trust-tier hardening.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use prost_types::value::Kind;
use prost_types::{Struct, Value};
use tempfile::tempdir;
use vam_inference::SessionBudget;
use vam_orchestrator::error::OrchestratorError;
use vam_orchestrator::host_tools::HostToolRegistry;
use vam_orchestrator::hsp::{HspClient, HspEndpoint};
use vam_orchestrator::hsp_echo::{spawn_counting_echo_unix, spawn_echo_unix};
use vam_orchestrator::hsp_registry::HspRegistry;
use vam_orchestrator::hsp_store::seed_hsp_skills;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::security::SessionSecurityContext;
use vam_orchestrator::session::{builtin_tool_catalog, merge_tool_catalog};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_security::PolicyMode;
use vam_spec::v1::{
    CapabilityManifest, ExecuteHostToolRequest, HookParticipation, InvokeRequest,
    ManifestSecretBinding, RegisterRequest, ToolExport, ToolIntentRequest,
};
use vam_spec::validation::{host_tool_digest, Validate};

fn approved_manifest(broker_id: &str, trust_tier: i32) -> CapabilityManifest {
    CapabilityManifest {
        broker_id: broker_id.to_string(),
        trust_tier,
        secret_bindings: vec![ManifestSecretBinding {
            logical_name: "api-token".to_string(),
            vault_key: "vault/echo/api-token".to_string(),
            required: false,
        }],
        egress_domains: vec!["api.example.com".to_string()],
        hooks: vec![HookParticipation {
            phase: 1,
            required: false,
        }],
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

fn register_request(manifest: CapabilityManifest) -> RegisterRequest {
    RegisterRequest {
        broker_id: manifest.broker_id.clone(),
        broker_version: "phase6".to_string(),
        manifest: Some(manifest),
    }
}

fn invoke_request(tool_name: &str, args: Option<Struct>) -> InvokeRequest {
    InvokeRequest {
        session_id: "sess-1".to_string(),
        run_id: "run-1".to_string(),
        tool_name: tool_name.to_string(),
        args,
        resolved_secrets: HashMap::new(),
        context: None,
        trace_context: None,
        caller_context: None,
        run_grant: None,
        idempotency_key: String::new(),
    }
}

fn url_args(url: &str) -> Struct {
    Struct {
        fields: [(
            "url".to_string(),
            Value {
                kind: Some(Kind::StringValue(url.to_string())),
            },
        )]
        .into(),
    }
}

fn message_args(message: &str) -> Struct {
    Struct {
        fields: [(
            "message".to_string(),
            Value {
                kind: Some(Kind::StringValue(message.to_string())),
            },
        )]
        .into(),
    }
}

fn assert_hsp_denied(err: OrchestratorError, prefix: &str) {
    match err {
        OrchestratorError::Hsp(message) => assert!(
            message.starts_with(prefix),
            "expected prefix '{prefix}', got '{message}'"
        ),
        other => panic!("expected OrchestratorError::Hsp, got {other:?}"),
    }
}

fn security_ctx(session_id: &str) -> SessionSecurityContext {
    SessionSecurityContext::new(
        session_id.to_string(),
        "urn:vam:agent:hsp-trust".to_string(),
        r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"echo.ping"
);
"#,
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
async fn first_party_unix_register_succeeds() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let approved = approved_manifest("echo", 2);
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("echo.sock");
    let (_path, _handle) = spawn_echo_unix(sock.clone()).unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    let response = HspRegistry::new(&store)
        .register_admitted(&mut client, register_request(approved))
        .await
        .unwrap();
    assert!(response.error.is_none());
}

#[tokio::test]
async fn third_party_unix_register_denied() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let approved = approved_manifest("echo", 4);
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("unused.sock");
    // Do not spawn a broker — deny must happen before dial.
    let mut client = HspClient::new(HspEndpoint::Unix(sock));
    let err = HspRegistry::new(&store)
        .register_admitted(&mut client, register_request(approved))
        .await
        .unwrap_err();
    assert_hsp_denied(err, "hsp denied: vsock required");
}

#[tokio::test]
async fn core_unix_register_denied() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let approved = approved_manifest("echo", 1);
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("unused.sock");
    let mut client = HspClient::new(HspEndpoint::Unix(sock));
    let err = HspRegistry::new(&store)
        .register_admitted(&mut client, register_request(approved))
        .await
        .unwrap_err();
    assert_hsp_denied(err, "hsp denied: core tier is in-process");
}

#[test]
fn unspecified_tier_upsert_and_validate_rejected() {
    let mut manifest = approved_manifest("echo", 0);
    let err = manifest.validate().unwrap_err();
    assert!(err.field_violations.iter().any(|v| {
        v.field_path == "trust_tier" && v.description.contains("cannot be unspecified")
    }));

    let store = SqliteWorkflowStore::in_memory().unwrap();
    let upsert_err = store.upsert_hsp_broker_approval(&manifest).unwrap_err();
    match upsert_err {
        OrchestratorError::InvalidContract(message) => {
            assert!(message.contains("cannot be unspecified"), "got {message}");
        }
        other => panic!("expected InvalidContract, got {other:?}"),
    }

    // Register path also fail-closes via validate when approval somehow exists.
    manifest.trust_tier = 2;
    store.upsert_hsp_broker_approval(&manifest).unwrap();
    let mut presented = manifest.clone();
    presented.trust_tier = 0;
    let err = HspRegistry::new(&store)
        .admit_register(&register_request(presented))
        .unwrap_err();
    match err {
        OrchestratorError::InvalidContract(message) => {
            assert!(message.contains("cannot be unspecified"), "got {message}");
        }
        other => panic!("expected InvalidContract, got {other:?}"),
    }
}

#[test]
fn required_secret_empty_vault_key_rejected() {
    let mut manifest = approved_manifest("echo", 2);
    manifest.secret_bindings = vec![ManifestSecretBinding {
        logical_name: "api-token".to_string(),
        vault_key: String::new(),
        required: true,
    }];
    let err = manifest.validate().unwrap_err();
    assert!(err
        .field_violations
        .iter()
        .any(|v| v.description.contains("hsp denied: unbound secret")));

    let store = SqliteWorkflowStore::in_memory().unwrap();
    let upsert_err = store.upsert_hsp_broker_approval(&manifest).unwrap_err();
    match upsert_err {
        OrchestratorError::InvalidContract(message) => {
            assert!(
                message.contains("hsp denied: unbound secret"),
                "got {message}"
            );
        }
        other => panic!("expected InvalidContract, got {other:?}"),
    }
}

#[tokio::test]
async fn invoke_missing_required_vault_key_unbound() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let mut approved = approved_manifest("echo", 2);
    approved.secret_bindings = vec![ManifestSecretBinding {
        logical_name: "api-token".to_string(),
        vault_key: "vault/echo/missing".to_string(),
        required: true,
    }];
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let vault = SecretsVault::for_tests(store.clone());
    // Deliberately do not put vault/echo/missing.

    let dir = tempdir().unwrap();
    let sock = dir.path().join("echo.sock");
    let (_path, _handle) = spawn_echo_unix(sock.clone()).unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    HspRegistry::new(store.as_ref())
        .register_admitted(&mut client, register_request(approved))
        .await
        .unwrap();

    let skills_dir = dir.path().join("skills");
    let session_id = "sess-unbound";
    seed_session_catalog(store.as_ref(), session_id, &skills_dir);
    let ctx = security_ctx(session_id);
    let registry = HostToolRegistry::with_test_adapters();
    let args = message_args("ping");
    let token = authorize(
        &ctx,
        store.as_ref(),
        "echo.ping",
        "hosted:echo:echo.ping",
        args.clone(),
    );

    let resp = ctx
        .handle_execute_host_tool(
            store.as_ref(),
            Some(vault.as_ref()),
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
        resp.text_result
            .contains("hsp denied: unbound secret: api-token"),
        "got {}",
        resp.text_result
    );
}

#[tokio::test]
async fn invoke_url_not_allowlisted_no_dial() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let approved = approved_manifest("echo", 2);
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("count.sock");
    let invokes = Arc::new(AtomicU64::new(0));
    let (_path, _handle) = spawn_counting_echo_unix(sock.clone(), invokes.clone()).unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    HspRegistry::new(&store)
        .register_admitted(&mut client, register_request(approved))
        .await
        .unwrap();

    let err = HspRegistry::new(&store)
        .invoke_admitted(
            "echo",
            &mut client,
            invoke_request("echo.ping", Some(url_args("https://evil.example.com/x"))),
        )
        .await
        .unwrap_err();
    assert_hsp_denied(err, "hsp denied: extra domain:");
    assert_eq!(invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn invoke_loopback_url_ssrf_restricted() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let mut approved = approved_manifest("echo", 2);
    approved.egress_domains = vec!["127.0.0.1".to_string()];
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("count.sock");
    let invokes = Arc::new(AtomicU64::new(0));
    let (_path, _handle) = spawn_counting_echo_unix(sock.clone(), invokes.clone()).unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    HspRegistry::new(&store)
        .register_admitted(&mut client, register_request(approved))
        .await
        .unwrap();

    let err = HspRegistry::new(&store)
        .invoke_admitted(
            "echo",
            &mut client,
            invoke_request("echo.ping", Some(url_args("http://127.0.0.1/secret"))),
        )
        .await
        .unwrap_err();
    assert_hsp_denied(err, "hsp denied: ssrf restricted:");
    assert_eq!(invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn invoke_allowlisted_url_reaches_broker() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let approved = approved_manifest("echo", 2);
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("count.sock");
    let invokes = Arc::new(AtomicU64::new(0));
    let (_path, _handle) = spawn_counting_echo_unix(sock.clone(), invokes.clone()).unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    HspRegistry::new(&store)
        .register_admitted(&mut client, register_request(approved))
        .await
        .unwrap();

    let response = HspRegistry::new(&store)
        .invoke_admitted(
            "echo",
            &mut client,
            invoke_request(
                "echo.ping",
                Some(url_args("https://api.example.com/v1/items")),
            ),
        )
        .await
        .unwrap();
    assert!(response.result.is_some());
    assert_eq!(invokes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invoke_echo_ping_without_urls_succeeds() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let approved = approved_manifest("echo", 2);
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("echo.sock");
    let (_path, _handle) = spawn_echo_unix(sock.clone()).unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    HspRegistry::new(&store)
        .register_admitted(&mut client, register_request(approved))
        .await
        .unwrap();

    let invoke = HspRegistry::new(&store)
        .invoke_admitted(
            "echo",
            &mut client,
            invoke_request("echo.ping", Some(message_args("hello"))),
        )
        .await
        .unwrap();
    let output = invoke.result.unwrap().output.unwrap();
    let tool_name = output.fields.get("tool_name").unwrap();
    assert!(matches!(
        tool_name.kind,
        Some(Kind::StringValue(ref name)) if name == "echo.ping"
    ));
}

#[tokio::test]
async fn third_party_stored_endpoint_invoke_denied() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let approved = approved_manifest("echo", 4);
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("count.sock");
    let invokes = Arc::new(AtomicU64::new(0));
    let (_path, _handle) = spawn_counting_echo_unix(sock.clone(), invokes.clone()).unwrap();
    // Bypass register_admitted (which would deny) and plant a UDS endpoint.
    store
        .set_hsp_broker_endpoint("echo", &HspEndpoint::Unix(sock.clone()))
        .unwrap();

    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    let err = HspRegistry::new(&store)
        .invoke_admitted(
            "echo",
            &mut client,
            invoke_request("echo.ping", Some(message_args("nope"))),
        )
        .await
        .unwrap_err();
    assert_hsp_denied(err, "hsp denied: vsock required");
    assert_eq!(invokes.load(Ordering::SeqCst), 0);
}
