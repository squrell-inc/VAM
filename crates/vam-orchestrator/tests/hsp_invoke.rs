//! Integration tests for HSP ExecuteHostTool → Invoke (Phase 4).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use prost_types::Struct;
use tempfile::tempdir;
use vam_inference::SessionBudget;
use vam_orchestrator::host_tools::HostToolRegistry;
use vam_orchestrator::hsp::{HspClient, HspEndpoint};
use vam_orchestrator::hsp_echo::{
    spawn_counting_echo_unix, spawn_echo_unix, spawn_hanging_echo_unix,
};
use vam_orchestrator::hsp_registry::HspRegistry;
use vam_orchestrator::hsp_store::seed_hsp_skills;
use vam_orchestrator::secrets::{SecretKind, SecretsVault};
use vam_orchestrator::security::SessionSecurityContext;
use vam_orchestrator::session::{builtin_tool_catalog, merge_tool_catalog};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_security::PolicyMode;
use vam_spec::v1::{
    CapabilityManifest, ExecuteHostToolRequest, HookParticipation, ManifestSecretBinding,
    RegisterRequest, ToolExport, ToolIntentRequest,
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

fn approved_manifest_with_required_secret(broker_id: &str) -> CapabilityManifest {
    let mut manifest = approved_manifest(broker_id);
    manifest.secret_bindings = vec![ManifestSecretBinding {
        logical_name: "api-token".to_string(),
        vault_key: "vault/echo/api-token".to_string(),
        required: true,
    }];
    manifest
}

fn register_request(manifest: CapabilityManifest) -> RegisterRequest {
    RegisterRequest {
        broker_id: manifest.broker_id.clone(),
        broker_version: "phase4".to_string(),
        manifest: Some(manifest),
    }
}

fn security_ctx(session_id: &str, policy: &str) -> SessionSecurityContext {
    SessionSecurityContext::new(
        session_id.to_string(),
        "urn:vam:agent:hsp-invoke".to_string(),
        policy,
        PolicyMode::Enforce,
        1,
        ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]),
        None,
        Arc::new(SessionBudget::new(None)),
    )
    .expect("security context")
}

fn allow_echo_policy() -> &'static str {
    r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"echo.ping"
);
"#
}

fn deny_echo_policy() -> &'static str {
    r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"other.tool"
);
"#
}

fn seed_session_catalog(
    store: &SqliteWorkflowStore,
    session_id: &str,
    skills_dir: &std::path::Path,
) {
    seed_session_catalog_for_owner(store, session_id, skills_dir, "local");
}

fn seed_session_catalog_for_owner(
    store: &SqliteWorkflowStore,
    session_id: &str,
    skills_dir: &std::path::Path,
    owner_user_id: &str,
) {
    let _ = store.create_user(owner_user_id, owner_user_id);
    store
        .create_session_with_owner(
            session_id,
            None,
            "vm-test",
            &format!("{owner_user_id}--ws-test"),
            "storage-default",
            owner_user_id,
        )
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

fn ping_args(message: &str) -> Struct {
    let mut args = Struct::default();
    args.fields.insert(
        "message".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(message.to_string())),
        },
    );
    args
}

#[tokio::test]
async fn execute_host_tool_invokes_echo_broker() {
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
    HspRegistry::new(&store)
        .register_admitted(&mut client, register_request(approved))
        .await
        .unwrap();
    assert!(store.get_hsp_broker_endpoint("echo").unwrap().is_some());

    let skills_dir = dir.path().join("skills");
    let session_id = "sess-hsp-happy";
    seed_session_catalog(&store, session_id, &skills_dir);

    let ctx = security_ctx(session_id, allow_echo_policy());
    let registry = HostToolRegistry::with_test_adapters();
    let args = ping_args("hello-hsp");
    let token = authorize(
        &ctx,
        &store,
        "echo.ping",
        "hosted:echo:echo.ping",
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

    assert!(resp.error.is_none(), "{:?}", resp.error);
    assert!(
        !resp.text_result.contains("hsp invoke not wired"),
        "{}",
        resp.text_result
    );
    assert!(
        resp.text_result.contains("echo.ping"),
        "{}",
        resp.text_result
    );
    assert!(
        resp.text_result.contains("hello-hsp"),
        "{}",
        resp.text_result
    );
}

#[tokio::test]
async fn execute_host_tool_invokes_echo_ts_broker() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let approved = approved_manifest("echo-ts");
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("echo-ts.sock");
    let (_path, _handle) = spawn_echo_unix(sock.clone()).unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    HspRegistry::new(&store)
        .register_admitted(&mut client, register_request(approved))
        .await
        .unwrap();
    assert!(store.get_hsp_broker_endpoint("echo-ts").unwrap().is_some());

    let skills_dir = dir.path().join("skills");
    let session_id = "sess-hsp-echo-ts";
    seed_session_catalog(&store, session_id, &skills_dir);

    let skill_dir = skills_dir.join("hosted-echo-ts");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(skill_dir.join("manifest.json")).unwrap())
            .unwrap();
    let tool = &manifest["tools"][0];
    assert_eq!(tool["name"], "echo.ping");
    assert_eq!(tool["host_exec"]["adapter_id"], "hosted:echo-ts:echo.ping");
    let skills_md = std::fs::read_to_string(skill_dir.join("SKILLS.md")).unwrap();
    assert!(skills_md.contains("name: \"hosted-echo-ts\""));

    let ctx = security_ctx(session_id, allow_echo_policy());
    let registry = HostToolRegistry::with_test_adapters();
    let args = ping_args("hello-hsp");
    let token = authorize(
        &ctx,
        &store,
        "echo.ping",
        "hosted:echo-ts:echo.ping",
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

    assert!(resp.error.is_none(), "{:?}", resp.error);
    assert!(
        resp.text_result.contains("echo.ping"),
        "{}",
        resp.text_result
    );
    assert!(
        resp.text_result.contains("hello-hsp"),
        "{}",
        resp.text_result
    );
}

#[tokio::test]
async fn cedar_deny_never_dials_broker() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let approved = approved_manifest("echo");
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let invokes = Arc::new(AtomicU64::new(0));
    let dir = tempdir().unwrap();
    let sock = dir.path().join("count.sock");
    let (_path, _handle) = spawn_counting_echo_unix(sock.clone(), invokes.clone()).unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    HspRegistry::new(&store)
        .register_admitted(&mut client, register_request(approved))
        .await
        .unwrap();

    let skills_dir = dir.path().join("skills");
    let session_id = "sess-hsp-cedar-deny";
    seed_session_catalog(&store, session_id, &skills_dir);

    let ctx = security_ctx(session_id, deny_echo_policy());
    let registry = HostToolRegistry::with_test_adapters();
    let args = ping_args("nope");

    let intent = ctx.handle_tool_intent_request(
        &store,
        ToolIntentRequest {
            lease: None,
            tool_name: "echo.ping".to_string(),
            arguments: Some(args.clone()),
            arguments_digest: None,
            script_digest: Some(vam_spec::v1::Digest {
                algorithm: 1,
                hex_hash: host_tool_digest("hosted:echo:echo.ping"),
            }),
            call_id: String::new(),
        },
    );
    assert_eq!(
        intent.disposition,
        vam_spec::v1::tool_intent_response::Disposition::Denied as i32
    );
    assert_eq!(invokes.load(Ordering::SeqCst), 0);

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
                capability_token: None,
                call_id: String::new(),
            },
        )
        .await;
    assert!(resp.error.is_some());
    assert_eq!(invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn execute_without_token_or_replay_never_dials() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let approved = approved_manifest("echo");
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let invokes = Arc::new(AtomicU64::new(0));
    let dir = tempdir().unwrap();
    let sock = dir.path().join("replay.sock");
    let (_path, _handle) = spawn_counting_echo_unix(sock.clone(), invokes.clone()).unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    HspRegistry::new(&store)
        .register_admitted(&mut client, register_request(approved))
        .await
        .unwrap();

    let skills_dir = dir.path().join("skills");
    let session_id = "sess-hsp-token";
    seed_session_catalog(&store, session_id, &skills_dir);
    let ctx = security_ctx(session_id, allow_echo_policy());
    let registry = HostToolRegistry::with_test_adapters();
    let args = ping_args("once");

    let missing = ctx
        .handle_execute_host_tool(
            &store,
            None,
            &registry,
            &skills_dir,
            ExecuteHostToolRequest {
                lease: None,
                tool_name: "echo.ping".to_string(),
                arguments: Some(args.clone()),
                capability_token: None,
                call_id: String::new(),
            },
        )
        .await;
    assert!(missing.error.is_some());
    assert_eq!(invokes.load(Ordering::SeqCst), 0);

    let token = authorize(
        &ctx,
        &store,
        "echo.ping",
        "hosted:echo:echo.ping",
        args.clone(),
    );
    let first = ctx
        .handle_execute_host_tool(
            &store,
            None,
            &registry,
            &skills_dir,
            ExecuteHostToolRequest {
                lease: None,
                tool_name: "echo.ping".to_string(),
                arguments: Some(args.clone()),
                capability_token: Some(token.clone()),
                call_id: String::new(),
            },
        )
        .await;
    assert!(first.error.is_none());
    assert_eq!(invokes.load(Ordering::SeqCst), 1);

    let replay = ctx
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
    assert!(replay.error.is_some());
    assert_eq!(invokes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn vault_secret_injected_and_redacted() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let approved = approved_manifest_with_required_secret("echo");
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let vault = SecretsVault::for_tests(store.clone());
    vault
        .put_secret(
            "vault/echo/api-token",
            "sk-super-secret-token",
            SecretKind::Egress,
            &[],
            None,
        )
        .unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("secret.sock");
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
    let session_id = "sess-hsp-secret";
    seed_session_catalog(store.as_ref(), session_id, &skills_dir);
    let ctx = security_ctx(session_id, allow_echo_policy());
    let registry = HostToolRegistry::with_test_adapters();
    let args = ping_args("with-secret");
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

    assert!(resp.error.is_none(), "{:?}", resp.error);
    assert!(
        resp.text_result.contains("***"),
        "expected redaction marker: {}",
        resp.text_result
    );
    assert!(
        !resp.text_result.contains("sk-super-secret-token"),
        "plaintext leaked: {}",
        resp.text_result
    );
}

#[tokio::test]
async fn hanging_broker_times_out_fail_closed() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let approved = approved_manifest("echo");
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("hang.sock");
    let (_path, _handle) = spawn_hanging_echo_unix(sock.clone()).unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    HspRegistry::new(&store)
        .register_admitted(&mut client, register_request(approved))
        .await
        .unwrap();

    let skills_dir = dir.path().join("skills");
    let session_id = "sess-hsp-timeout";
    seed_session_catalog(&store, session_id, &skills_dir);
    let ctx = security_ctx(session_id, allow_echo_policy());
    let registry = HostToolRegistry::with_test_adapters();
    let args = ping_args("hang");
    let token = authorize(
        &ctx,
        &store,
        "echo.ping",
        "hosted:echo:echo.ping",
        args.clone(),
    );

    // side_effect=0 → light envelope → 10s wall timeout
    let started = std::time::Instant::now();
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
    let elapsed = started.elapsed();

    assert!(
        resp.text_result.starts_with("tool_error:"),
        "{}",
        resp.text_result
    );
    assert!(
        resp.text_result.to_lowercase().contains("timed out")
            || resp.text_result.to_lowercase().contains("timeout"),
        "{}",
        resp.text_result
    );
    assert!(
        elapsed < Duration::from_secs(12),
        "timeout should fire near light envelope (10s), took {:?}",
        elapsed
    );
}

#[tokio::test]
async fn missing_endpoint_denies_without_dial() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let approved = approved_manifest("echo");
    store.upsert_hsp_broker_approval(&approved).unwrap();
    // Snapshot tools without register_admitted → no endpoint row.
    store
        .replace_hsp_broker_tools(
            "echo",
            &[vam_orchestrator::hsp_store::HspBrokerToolSnapshot {
                tool_name: "echo.ping".to_string(),
                adapter_id: "hosted:echo:echo.ping".to_string(),
                side_effect: 0,
                description: "Ping".to_string(),
                input_schema_json: "{}".to_string(),
            }],
        )
        .unwrap();
    assert!(store.get_hsp_broker_endpoint("echo").unwrap().is_none());

    let dir = tempdir().unwrap();
    let skills_dir = dir.path().join("skills");
    let session_id = "sess-hsp-no-endpoint";
    seed_session_catalog(&store, session_id, &skills_dir);
    let ctx = security_ctx(session_id, allow_echo_policy());
    let registry = HostToolRegistry::with_test_adapters();
    let args = ping_args("no-endpoint");
    let token = authorize(
        &ctx,
        &store,
        "echo.ping",
        "hosted:echo:echo.ping",
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
        resp.text_result.contains("hsp denied: no endpoint"),
        "{}",
        resp.text_result
    );
}

#[test]
fn skill_seed_writes_hosted_manifest_and_skills_md() {
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
                description: "Echo ping".to_string(),
                input_schema_json:
                    r#"{"type":"object","properties":{"message":{"type":"string"}}}"#.to_string(),
            }],
        )
        .unwrap();

    let dir = tempdir().unwrap();
    let skills_dir = dir.path().join("skills");
    seed_hsp_skills(&skills_dir, &store, true).unwrap();

    let skill_dir = skills_dir.join("hosted-echo");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(skill_dir.join("manifest.json")).unwrap())
            .unwrap();
    let tool = &manifest["tools"][0];
    assert_eq!(tool["name"], "echo.ping");
    assert_eq!(tool["host_exec"]["adapter_id"], "hosted:echo:echo.ping");
    assert!(tool["input_schema_json"]
        .as_str()
        .unwrap()
        .contains("message"));

    let skills_md = std::fs::read_to_string(skill_dir.join("SKILLS.md")).unwrap();
    assert!(skills_md.contains("name: \"hosted-echo\""));
    assert!(skills_md.contains("description:"));
}

#[test]
fn hosted_adapter_still_resolves_via_registry_prefix() {
    let registry = HostToolRegistry::with_test_adapters();
    let adapter = registry.get("hosted:echo:echo.ping").unwrap();
    let ctx = vam_orchestrator::host_tools::HostToolContext {
        session_id: "sess",
        tool_name: "echo.ping",
        args: &Struct::default(),
        config: &HashMap::new(),
        required_secrets: &[],
        vault: None,
        caller_user_id: "",
        timeout_ms: 30_000,
        idempotency_key: None,
    };
    // Direct shim invoke remains a stub; ExecuteHostTool uses the async HSP path instead.
    let err = adapter.invoke(&ctx).unwrap_err();
    assert_eq!(err, "hsp invoke not wired");
}

async fn register_echo(
    store: &SqliteWorkflowStore,
    sock: std::path::PathBuf,
    manifest: CapabilityManifest,
) {
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    HspRegistry::new(store)
        .register_admitted(&mut client, register_request(manifest))
        .await
        .unwrap();
}

fn top_level_string(output: &str, key: &str) -> Option<String> {
    let body = vam_security::vendor_text::fenced_body(output)?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    value.get(key)?.as_str().map(str::to_string)
}

#[tokio::test]
async fn guest_cannot_override_caller_identity_on_invoke() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let approved = approved_manifest("echo");
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("identity.sock");
    let (_path, _handle) = spawn_echo_unix(sock.clone()).unwrap();
    register_echo(store.as_ref(), sock, approved).await;

    let skills_dir = dir.path().join("skills");
    let session_id = "sess-hsp-identity";
    seed_session_catalog_for_owner(store.as_ref(), session_id, &skills_dir, "alice");
    store
        .persist_run_spec_with_owner(
            "run-alice",
            "{}",
            Some(session_id),
            "urn:vam:agent:hsp-invoke",
            None,
            "User::\"alice\"",
            "alice",
        )
        .unwrap();

    let ctx = security_ctx(session_id, allow_echo_policy());
    let registry = HostToolRegistry::with_test_adapters();
    let mut args = ping_args("hello");
    args.fields.insert(
        "user_id".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue("attacker".into())),
        },
    );
    args.fields.insert(
        "caller_user_id".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue("attacker".into())),
        },
    );
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

    assert!(resp.error.is_none(), "{:?}", resp.error);
    assert_eq!(
        top_level_string(&resp.text_result, "caller_user_id").as_deref(),
        Some("alice")
    );
    let body = vam_security::vendor_text::fenced_body(&resp.text_result).expect("fenced");
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        parsed
            .pointer("/args/caller_user_id")
            .and_then(|v| v.as_str()),
        Some("attacker"),
        "guest arg must still be present and must not become the stamped caller: {}",
        resp.text_result
    );
}

#[tokio::test]
async fn alice_invoke_does_not_inject_bobs_user_secret() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let approved = approved_manifest_with_required_secret("echo");
    store.upsert_hsp_broker_approval(&approved).unwrap();

    store.create_user("alice", "alice").unwrap();
    store.create_user("bob", "bob").unwrap();
    let vault = SecretsVault::for_tests(store.clone());
    vault
        .put_secret_with_owner(
            "vault/echo/api-token",
            "alice-secret-token",
            SecretKind::Egress,
            &[],
            None,
            "alice",
        )
        .unwrap();
    vault
        .put_secret_with_owner(
            "vault/echo/api-token",
            "bob-secret-token",
            SecretKind::Egress,
            &[],
            None,
            "bob",
        )
        .unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("scoped.sock");
    let (_path, _handle) = spawn_echo_unix(sock.clone()).unwrap();
    register_echo(store.as_ref(), sock, approved).await;

    let skills_dir = dir.path().join("skills");
    let session_id = "sess-hsp-alice-secret";
    seed_session_catalog_for_owner(store.as_ref(), session_id, &skills_dir, "alice");
    let ctx = security_ctx(session_id, allow_echo_policy());
    let registry = HostToolRegistry::with_test_adapters();
    let args = ping_args("scoped");
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

    assert!(resp.error.is_none(), "{:?}", resp.error);
    assert!(
        resp.text_result.contains("***"),
        "alice secret should be injected then redacted: {}",
        resp.text_result
    );
    assert!(
        !resp.text_result.contains("alice-secret-token"),
        "alice plaintext leaked: {}",
        resp.text_result
    );
    assert!(
        !resp.text_result.contains("bob-secret-token"),
        "bob secret injected into alice invoke: {}",
        resp.text_result
    );
    assert_eq!(
        top_level_string(&resp.text_result, "caller_user_id").as_deref(),
        Some("alice")
    );
}

#[tokio::test]
async fn broker_sees_alice_user_id_on_invoke() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let approved = approved_manifest("echo");
    store.upsert_hsp_broker_approval(&approved).unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("broker-user.sock");
    let (_path, _handle) = spawn_echo_unix(sock.clone()).unwrap();
    register_echo(store.as_ref(), sock, approved).await;

    let skills_dir = dir.path().join("skills");
    let session_id = "sess-hsp-broker-user";
    seed_session_catalog_for_owner(store.as_ref(), session_id, &skills_dir, "alice");
    let ctx = security_ctx(session_id, allow_echo_policy());
    let registry = HostToolRegistry::with_test_adapters();
    let args = ping_args("whoami");
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

    assert!(resp.error.is_none(), "{:?}", resp.error);
    assert_eq!(
        top_level_string(&resp.text_result, "caller_user_id").as_deref(),
        Some("alice")
    );
}
