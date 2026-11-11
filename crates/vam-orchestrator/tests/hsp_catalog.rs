//! Integration tests for HSP ListTools catalog merge (Phase 3).

use std::sync::Arc;
use std::time::Duration;

use tempfile::tempdir;
use vam_inference::SessionBudget;
use vam_orchestrator::error::OrchestratorError;
use vam_orchestrator::host_tools::HostToolRegistry;
use vam_orchestrator::hsp::{HspClient, HspEndpoint};
use vam_orchestrator::hsp_echo::{spawn_echo_unix, spawn_extra_tool_echo_unix};
use vam_orchestrator::hsp_registry::HspRegistry;
use vam_orchestrator::security::SessionSecurityContext;
use vam_orchestrator::session::{builtin_tool_catalog, merge_tool_catalog, ToolCatalogEntry};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_security::PolicyMode;
use vam_spec::v1::{
    CapabilityManifest, HookParticipation, ManifestSecretBinding, RegisterRequest, ToolExport,
};
use vam_spec::validation::host_tool_digest;

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
        broker_version: "phase3".to_string(),
        manifest: Some(manifest),
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

fn hosted_entry(tool_name: &str, broker_id: &str) -> ToolCatalogEntry {
    let adapter_id = format!("hosted:{broker_id}:{tool_name}");
    ToolCatalogEntry {
        tool_name: tool_name.to_string(),
        command: "host".to_string(),
        entrypoint: adapter_id.clone(),
        digest: host_tool_digest(&adapter_id),
        side_effect: 0,
        skill_name: String::new(),
        approval_required: false,
    }
}

#[tokio::test]
async fn register_admitted_persists_hosted_adapter_snapshot() {
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
        ..approved
    };
    registry
        .register_admitted(&mut client, register_request(presented))
        .await
        .unwrap();

    let snapshot = store.list_hsp_broker_tools("echo").unwrap();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].tool_name, "echo.ping");
    assert_eq!(snapshot[0].adapter_id, "hosted:echo:echo.ping");
}

#[test]
fn merge_builtins_and_hosted_includes_host_command() {
    let builtins = builtin_tool_catalog();
    let hosted = vec![hosted_entry("echo.ping", "echo")];
    let merged = merge_tool_catalog(builtins, vec![], hosted).unwrap();
    let entry = merged
        .iter()
        .find(|entry| entry.tool_name == "echo.ping")
        .expect("hosted tool merged");
    assert_eq!(entry.command, "host");
    assert_eq!(entry.entrypoint, "hosted:echo:echo.ping");
}

#[test]
fn merge_rejects_hosted_builtin_name_collision() {
    let builtins = builtin_tool_catalog();
    let hosted = vec![hosted_entry("read_file", "echo")];
    assert!(merge_tool_catalog(builtins, vec![], hosted).is_err());
}

#[test]
fn merge_rejects_hosted_skill_name_collision() {
    let builtins = builtin_tool_catalog();
    let skills = vec![ToolCatalogEntry {
        tool_name: "calculator".to_string(),
        command: "bun".to_string(),
        entrypoint: "execute.js".to_string(),
        digest: "abc".to_string(),
        side_effect: 1,
        skill_name: String::new(),
        approval_required: false,
    }];
    let hosted = vec![hosted_entry("calculator", "echo")];
    assert!(merge_tool_catalog(builtins, skills, hosted).is_err());
}

#[test]
fn advertised_tool_names_return_tool_name_not_adapter_id() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let session_id = "sess-hsp-adv";
    store
        .create_session(session_id, None, "vm-test", "ws-test", "storage-default")
        .unwrap();

    let mut catalog = builtin_tool_catalog();
    catalog.push(hosted_entry("echo.ping", "echo"));
    store.record_tool_catalog(session_id, &catalog).unwrap();

    let policy = r#"
permit (
    principal == Agent::"urn:vam:agent:hsp-test",
    action == Action::"ExecuteTool",
    resource == Tool::"echo.ping"
);
"#;
    let ctx = SessionSecurityContext::new(
        session_id.to_string(),
        "urn:vam:agent:hsp-test".to_string(),
        policy,
        PolicyMode::Enforce,
        1,
        ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]),
        None,
        Arc::new(SessionBudget::new(None)),
    )
    .expect("security context");

    let advertised = ctx.advertised_tool_names(&store);
    assert!(advertised.contains(&"echo.ping".to_string()));
    assert!(!advertised.iter().any(|name| name.starts_with("hosted:")));
}

#[tokio::test]
async fn list_tools_extra_undeclared_name_denies_and_leaves_snapshot_unchanged() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    store
        .upsert_hsp_broker_approval(&approved_manifest("echo"))
        .unwrap();
    store
        .replace_hsp_broker_tools(
            "echo",
            &[vam_orchestrator::hsp_store::HspBrokerToolSnapshot {
                tool_name: "echo.ping".to_string(),
                adapter_id: "hosted:echo:echo.ping".to_string(),
                side_effect: 0,
                description: String::new(),
                input_schema_json: "{}".to_string(),
            }],
        )
        .unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("extra.sock");
    let (_path, _handle) = spawn_extra_tool_echo_unix(sock.clone()).unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    let registry = HspRegistry::new(&store);

    let err = registry
        .list_tools_admitted(&mut client, "echo")
        .await
        .unwrap_err();
    assert_hsp_denied(err, "hsp denied: undeclared tool");

    let snapshot = store.list_hsp_broker_tools("echo").unwrap();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].adapter_id, "hosted:echo:echo.ping");
}

#[test]
fn disabled_approval_omits_hosted_overlay() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    store
        .upsert_hsp_broker_approval(&approved_manifest("echo"))
        .unwrap();
    store
        .replace_hsp_broker_tools(
            "echo",
            &[vam_orchestrator::hsp_store::HspBrokerToolSnapshot {
                tool_name: "echo.ping".to_string(),
                adapter_id: "hosted:echo:echo.ping".to_string(),
                side_effect: 0,
                description: String::new(),
                input_schema_json: "{}".to_string(),
            }],
        )
        .unwrap();
    store.disable_hsp_broker_approval("echo").unwrap();

    assert!(store.list_hsp_catalog_entries(true).unwrap().is_empty());
}

#[test]
fn hosted_tool_registry_prefix_lookup_returns_shim() {
    let registry = HostToolRegistry::with_test_adapters();
    assert!(registry.get("hosted:echo.broker:echo.ping").is_some());
    assert!(registry.get("echo").is_some());
    assert!(registry.get("unknown").is_none());
}

#[test]
fn hosted_tool_registry_invoke_shim_still_returns_not_wired() {
    // Direct adapter.invoke remains a stub. Phase 4 wires HSP Invoke through
    // SessionSecurityContext::handle_execute_host_tool, not the unit-struct shim.
    let registry = HostToolRegistry::with_test_adapters();
    let adapter = registry.get("hosted:echo:echo.ping").unwrap();
    let ctx = vam_orchestrator::host_tools::HostToolContext {
        session_id: "sess",
        tool_name: "echo.ping",
        args: &prost_types::Struct::default(),
        config: &std::collections::HashMap::new(),
        required_secrets: &[],
        vault: None,
        caller_user_id: "",
        timeout_ms: 30_000,
        idempotency_key: None,
    };
    let err = adapter.invoke(&ctx).unwrap_err();
    assert_eq!(err, "hsp invoke not wired");
}
