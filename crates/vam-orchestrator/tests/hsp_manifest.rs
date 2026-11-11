//! Integration tests for HSP capability manifest admission (Phase 2).

use std::collections::HashMap;
use std::time::Duration;

use prost_types::value::Kind;
use tempfile::tempdir;
use vam_orchestrator::error::OrchestratorError;
use vam_orchestrator::hsp::{HspClient, HspEndpoint};
use vam_orchestrator::hsp_echo::spawn_echo_unix;
use vam_orchestrator::hsp_registry::HspRegistry;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_spec::v1::{
    CapabilityManifest, HookParticipation, InvokeRequest, ManifestSecretBinding, RegisterRequest,
    ToolExport,
};

fn approved_manifest(broker_id: &str) -> CapabilityManifest {
    CapabilityManifest {
        broker_id: broker_id.to_string(),
        trust_tier: 2,
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
        tools: vec![
            ToolExport {
                tool_name: "echo.ping".to_string(),
                side_effect: 0,
                required_secrets: vec![],
                destination_policy: Default::default(),
                effect: None,
            },
            ToolExport {
                tool_name: "echo.status".to_string(),
                side_effect: 0,
                required_secrets: vec![],
                destination_policy: Default::default(),
                effect: None,
            },
        ],
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
        broker_version: "phase2".to_string(),
        manifest: Some(manifest),
    }
}

fn invoke_request(tool_name: &str, secrets: HashMap<String, String>) -> InvokeRequest {
    InvokeRequest {
        session_id: "sess-1".to_string(),
        run_id: "run-1".to_string(),
        tool_name: tool_name.to_string(),
        args: Some(prost_types::Struct {
            fields: [(
                "message".to_string(),
                prost_types::Value {
                    kind: Some(Kind::StringValue("hello".to_string())),
                },
            )]
            .into(),
        }),
        resolved_secrets: secrets,
        context: None,
        trace_context: None,
        caller_context: None,
        run_grant: None,
        idempotency_key: String::new(),
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

#[tokio::test]
async fn register_without_approval_is_denied() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let registry = HspRegistry::new(&store);
    let request = register_request(approved_manifest("echo"));
    let err = registry.admit_register(&request).unwrap_err();
    assert_hsp_denied(err, "hsp denied: unapproved broker");
}

#[tokio::test]
async fn approved_subset_register_and_invoke_succeed() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let approved = approved_manifest("echo");
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("echo.sock");
    let (_path, _handle) = spawn_echo_unix(sock.clone()).unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    let registry = HspRegistry::new(&store);

    let presented = CapabilityManifest {
        tools: vec![ToolExport {
            tool_name: "echo.ping".to_string(),
            side_effect: 0,
            required_secrets: vec![],
            destination_policy: Default::default(),
            effect: None,
        }],
        ..approved.clone()
    };
    let register = registry
        .register_admitted(&mut client, register_request(presented))
        .await
        .unwrap();
    assert!(register.error.is_none());

    let invoke = registry
        .invoke_admitted(
            "echo",
            &mut client,
            invoke_request("echo.ping", HashMap::new()),
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
async fn register_extra_tool_is_denied() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    store
        .upsert_hsp_broker_approval(&approved_manifest("echo"))
        .unwrap();
    let registry = HspRegistry::new(&store);

    let mut presented = approved_manifest("echo");
    presented.tools.push(ToolExport {
        tool_name: "echo.extra".to_string(),
        side_effect: 0,
        required_secrets: vec![],
        destination_policy: Default::default(),
        effect: None,
    });
    let err = registry
        .admit_register(&register_request(presented))
        .unwrap_err();
    assert_hsp_denied(err, "hsp denied: drifted manifest");
}

#[tokio::test]
async fn register_extra_secret_binding_is_denied() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    store
        .upsert_hsp_broker_approval(&approved_manifest("echo"))
        .unwrap();
    let registry = HspRegistry::new(&store);

    let mut presented = approved_manifest("echo");
    presented.secret_bindings.push(ManifestSecretBinding {
        logical_name: "extra-secret".to_string(),
        vault_key: "vault/echo/extra".to_string(),
        required: false,
    });
    let err = registry
        .admit_register(&register_request(presented))
        .unwrap_err();
    assert_hsp_denied(err, "hsp denied: drifted manifest");
}

#[tokio::test]
async fn register_extra_egress_domain_is_denied() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    store
        .upsert_hsp_broker_approval(&approved_manifest("echo"))
        .unwrap();
    let registry = HspRegistry::new(&store);

    let mut presented = approved_manifest("echo");
    presented
        .egress_domains
        .push("evil.example.com".to_string());
    let err = registry
        .admit_register(&register_request(presented))
        .unwrap_err();
    assert_hsp_denied(err, "hsp denied: extra domain");
}

#[tokio::test]
async fn invoke_undeclared_tool_is_denied() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    store
        .upsert_hsp_broker_approval(&approved_manifest("echo"))
        .unwrap();
    let registry = HspRegistry::new(&store);
    let err = registry
        .admit_invoke("echo", &invoke_request("echo.unknown", HashMap::new()))
        .unwrap_err();
    assert_hsp_denied(err, "hsp denied: undeclared tool");
}

#[tokio::test]
async fn invoke_extra_resolved_secret_is_denied() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    store
        .upsert_hsp_broker_approval(&approved_manifest("echo"))
        .unwrap();
    let registry = HspRegistry::new(&store);
    let mut secrets = HashMap::new();
    secrets.insert("not-approved".to_string(), "value".to_string());
    let err = registry
        .admit_invoke("echo", &invoke_request("echo.ping", secrets))
        .unwrap_err();
    assert_hsp_denied(err, "hsp denied: extra secret");
}

#[tokio::test]
async fn disabled_approval_denies_register() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    store
        .upsert_hsp_broker_approval(&approved_manifest("echo"))
        .unwrap();
    store.disable_hsp_broker_approval("echo").unwrap();
    let registry = HspRegistry::new(&store);
    let err = registry
        .admit_register(&register_request(approved_manifest("echo")))
        .unwrap_err();
    assert_hsp_denied(err, "hsp denied: unapproved broker");
}
