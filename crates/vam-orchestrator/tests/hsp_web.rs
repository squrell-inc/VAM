//! AR-1.4 / AR-1.7: open-web admission, least-privilege secrets and the
//! per-agent web switch, through the real host-tool path with an in-process
//! scripted broker. DNS is injected; nothing touches the network.
// Test closures return the large library error type; src/ error types are out of scope for a lint wave.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use prost_types::Struct;
use tempfile::tempdir;
use vam_inference::SessionBudget;
use vam_orchestrator::host_tools::HostToolRegistry;
use vam_orchestrator::hsp::HspEndpoint;
use vam_orchestrator::hsp_echo::{spawn_scripted_invoke_unix, ScriptedInvokeBroker};
use vam_orchestrator::hsp_store::{seed_hsp_skills, HspBrokerToolSnapshot};
use vam_orchestrator::secrets::{SecretKind, SecretsVault};
use vam_orchestrator::security::SessionSecurityContext;
use vam_orchestrator::session::{builtin_tool_catalog, merge_tool_catalog};
use vam_orchestrator::web_admission::session_web_enabled;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_security::ssrf_guard::HostResolver;
use vam_security::PolicyMode;
use vam_spec::v1::{
    AgentPermissions, AgentSpec, CapabilityManifest, ExecuteHostToolRequest,
    ExecuteHostToolResponse, ExecutionLease, HostDefault, InternetPermissions, InvokeRequest,
    InvokeResponse, ManifestSecretBinding, PermissionDecision, PermissionRule, ResourceId,
    ResultSource, ResultSourceKind, ToolDestination, ToolExport, ToolIntentRequest, ToolResult,
    WebPermissions,
};
use vam_spec::validation::host_tool_digest;

const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-hsp-web";
const AGENT: &str = "urn:vam:agent:local.vam.dev/acme/hsp-web";
const SESSION: &str = "sess-hsp-web";
const BROKER: &str = "vam.web";
const BRAVE: &str = "BRAVE-KEY-0123456789-CANARY";
const OTHER: &str = "OTHER-TOKEN-9876543210-CANARY";

/// Names resolve by prefix: `localhost` is loopback, `private` is 10.0.0.1, `imds` is the metadata
/// address, `nodns` fails, anything else is public.
struct TestDns;
impl HostResolver for TestDns {
    fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        let ip: IpAddr = if host.starts_with("localhost") {
            "127.0.0.1".parse().unwrap()
        } else if host.starts_with("private") {
            "10.0.0.1".parse().unwrap()
        } else if host.starts_with("imds") {
            "169.254.169.254".parse().unwrap()
        } else if host.starts_with("nodns") {
            return Err(io::Error::other("no such host"));
        } else {
            "93.184.216.34".parse().unwrap()
        };
        Ok(vec![SocketAddr::new(ip, port)])
    }
}

fn json_struct(value: serde_json::Value) -> Struct {
    vam_spec::encoding::json_to_proto::<Struct>("google.protobuf.Struct", &value.to_string())
        .unwrap()
}

fn manifest() -> CapabilityManifest {
    let mut labels = HashMap::new();
    labels.insert("vam.capability".to_string(), "web".to_string());
    CapabilityManifest {
        broker_id: BROKER.to_string(),
        trust_tier: 2,
        secret_bindings: vec![
            ManifestSecretBinding {
                logical_name: "brave-api-key".to_string(),
                vault_key: "vault/web/brave".to_string(),
                required: true,
            },
            ManifestSecretBinding {
                logical_name: "other-token".to_string(),
                vault_key: "vault/web/other".to_string(),
                required: false,
            },
        ],
        egress_domains: vec!["api.search.brave.com".to_string()],
        hooks: vec![],
        tools: vec![
            ToolExport {
                tool_name: "web_search".to_string(),
                side_effect: 1,
                required_secrets: vec!["brave-api-key".to_string()],
                destination_policy: ToolDestination::Allowlist as i32,
                effect: None,
            },
            ToolExport {
                tool_name: "web_fetch".to_string(),
                side_effect: 1,
                required_secrets: vec![],
                destination_policy: ToolDestination::PublicWeb as i32,
                effect: None,
            },
        ],
        depends_on: vec![],
        labels,
        identity_requirement: None,
        run_request_permission: None,
        notification_channel: None,
    }
}

fn agent(web: Option<bool>, deny: &[&str]) -> AgentSpec {
    AgentSpec {
        resource_id: Some(ResourceId {
            urn: AGENT.to_string(),
        }),
        display_name: "web".to_string(),
        description: "hsp web".to_string(),
        role: "assistant".to_string(),
        instruction_source: Some(
            vam_spec::v1::agent_spec::InstructionSource::SystemInstructions("x".to_string()),
        ),
        permissions: web.map(|enabled| AgentPermissions {
            tools: None,
            internet: Some(InternetPermissions {
                default: HostDefault::Allow as i32,
                rules: deny
                    .iter()
                    .map(|host| PermissionRule {
                        target: host.to_string(),
                        decision: PermissionDecision::Deny as i32,
                    })
                    .collect(),
            }),
            advanced: vec![],
            web: Some(WebPermissions { enabled }),
        }),
        ..Default::default()
    }
}

#[derive(Clone, Debug)]
struct Seen {
    tool: String,
    secret_names: Vec<String>,
    secret_values: Vec<String>,
}

struct Fx {
    store: Arc<SqliteWorkflowStore>,
    vault: Arc<SecretsVault>,
    ctx: SessionSecurityContext,
    registry: HostToolRegistry,
    skills_dir: std::path::PathBuf,
    seen: Arc<Mutex<Vec<Seen>>>,
    _dir: tempfile::TempDir,
}

impl Fx {
    fn calls(&self, tool: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.tool == tool)
            .count()
    }
    fn prime(&self, text: &str) {
        self.store.record_prompt_context(RUN, text).unwrap();
    }
}

struct Opts {
    web: Option<bool>,
    deny: Vec<&'static str>,
    /// Record the catalog as if web were on, whatever the agent says.
    force_catalog: bool,
    put_other_secret: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            web: Some(true),
            deny: vec![],
            force_catalog: false,
            put_other_secret: true,
        }
    }
}

async fn fixture(
    opts: Opts,
    script: impl Fn(&InvokeRequest) -> Result<InvokeResponse, tonic::Status> + Send + Sync + 'static,
) -> Fx {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let approved = manifest();
    store.upsert_hsp_broker_approval(&approved).unwrap();
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let recorder = seen.clone();
    let vault = SecretsVault::for_tests(store.clone());
    vault
        .put_secret("vault/web/brave", BRAVE, SecretKind::Egress, &[], None)
        .unwrap();
    if opts.put_other_secret {
        vault
            .put_secret("vault/web/other", OTHER, SecretKind::Egress, &[], None)
            .unwrap();
    }
    let dir = tempdir().unwrap();
    let sock = dir.path().join("web.sock");
    let (_path, _handle) = spawn_scripted_invoke_unix(
        sock.clone(),
        ScriptedInvokeBroker::new(move |req| {
            let mut names: Vec<String> = req.resolved_secrets.keys().cloned().collect();
            names.sort();
            recorder.lock().unwrap().push(Seen {
                tool: req.tool_name.clone(),
                secret_names: names,
                secret_values: req.resolved_secrets.values().cloned().collect(),
            });
            script(req)
        }),
    )
    .unwrap();
    let snapshots: Vec<HspBrokerToolSnapshot> = ["web_search", "web_fetch"]
        .iter()
        .map(|name| HspBrokerToolSnapshot {
            tool_name: name.to_string(),
            adapter_id: format!("hosted:{BROKER}:{name}"),
            side_effect: 1,
            description: format!("{name} tool"),
            input_schema_json: "{}".to_string(),
        })
        .collect();
    store.replace_hsp_broker_tools(BROKER, &snapshots).unwrap();
    store
        .set_hsp_broker_endpoint(BROKER, &HspEndpoint::Unix(sock))
        .unwrap();

    let _ = store.create_user("local", "local");
    store
        .publish_agent_draft_with_owner(&agent(opts.web, &opts.deny), "local")
        .unwrap();
    store
        .release_agent(
            AGENT,
            &vam_spec::v1::Version {
                major: 1,
                ..Default::default()
            },
        )
        .unwrap();
    store
        .create_session_with_owner(
            SESSION,
            None,
            "vm-test",
            "local--ws-test",
            "storage-default",
            "local",
        )
        .unwrap();

    let skills_dir = dir.path().join("skills");
    let web_enabled = opts.force_catalog || session_web_enabled(&store, SESSION, Some(AGENT));
    seed_hsp_skills(&skills_dir, store.as_ref(), web_enabled).unwrap();
    let skills: Vec<_> = vam_orchestrator::session::parse_skills_catalog(&skills_dir)
        .into_iter()
        .filter(|entry| !entry.entrypoint.starts_with("hosted:"))
        .collect();
    let hosted = store.list_hsp_catalog_entries(web_enabled).unwrap();
    let catalog = merge_tool_catalog(builtin_tool_catalog(), skills, hosted).unwrap();
    store.record_tool_catalog(SESSION, &catalog).unwrap();

    let ctx = SessionSecurityContext::new(
        SESSION.to_string(),
        AGENT.to_string(),
        r#"
permit (principal, action == Action::"ExecuteTool", resource == Tool::"web_search");
permit (principal, action == Action::"ExecuteTool", resource == Tool::"web_fetch");
"#,
        PolicyMode::Enforce,
        1,
        ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]),
        None,
        Arc::new(SessionBudget::new(None)),
    )
    .unwrap()
    .with_effect_key_secret(vam_orchestrator::signing::NodeKeys::ephemeral().effect_key_secret())
    .with_web_resolver(Arc::new(TestDns));
    Fx {
        store,
        vault,
        ctx,
        registry: HostToolRegistry::with_test_adapters(),
        skills_dir,
        seen,
        _dir: dir,
    }
}

fn lease() -> ExecutionLease {
    ExecutionLease {
        run_id: Some(ResourceId {
            urn: RUN.to_string(),
        }),
        lifecycle_generation: 1,
        ..Default::default()
    }
}

async fn call(fx: &Fx, tool: &str, args: serde_json::Value) -> ExecuteHostToolResponse {
    let args = json_struct(args);
    let intent = fx.ctx.handle_tool_intent_request(
        fx.store.as_ref(),
        ToolIntentRequest {
            lease: Some(lease()),
            tool_name: tool.to_string(),
            arguments: Some(args.clone()),
            arguments_digest: None,
            script_digest: Some(vam_spec::v1::Digest {
                algorithm: 1,
                hex_hash: host_tool_digest(&format!("hosted:{BROKER}:{tool}")),
            }),
            call_id: String::new(),
        },
    );
    assert_eq!(
        intent.disposition,
        vam_spec::v1::tool_intent_response::Disposition::Authorized as i32,
        "{intent:?}"
    );
    fx.ctx
        .handle_execute_host_tool(
            fx.store.as_ref(),
            Some(fx.vault.as_ref()),
            &fx.registry,
            &fx.skills_dir,
            ExecuteHostToolRequest {
                lease: Some(lease()),
                tool_name: tool.to_string(),
                arguments: Some(args),
                capability_token: intent.capability_token,
                call_id: String::new(),
            },
        )
        .await
}

fn ok(output: serde_json::Value, sources: Vec<ResultSource>) -> InvokeResponse {
    InvokeResponse {
        result: Some(ToolResult {
            output: Some(json_struct(output)),
            error: None,
        }),
        output_blocks: vec![],
        artifact: None,
        sources,
    }
}

fn hit(url: &str) -> ResultSource {
    ResultSource {
        url: url.to_string(),
        title: "T".to_string(),
        published: String::new(),
        kind: ResultSourceKind::SearchResult as i32,
    }
}

fn refused(resp: &ExecuteHostToolResponse, tool: &str, code: &str) {
    assert!(
        resp.text_result
            .starts_with(&format!("tool_error: {tool}: {code}: ")),
        "{}",
        resp.text_result
    );
}

#[tokio::test]
async fn a_url_from_a_prior_search_result_is_fetched() {
    let fx = fixture(Opts::default(), |req| {
        if req.tool_name == "web_search" {
            Ok(ok(
                serde_json::json!({"results": []}),
                vec![hit("https://news.example/story?id=7")],
            ))
        } else {
            Ok(ok(serde_json::json!({"status": "ok"}), vec![]))
        }
    })
    .await;
    let search = call(&fx, "web_search", serde_json::json!({"query": "rust news"})).await;
    assert!(
        !search.text_result.starts_with("tool_error"),
        "{}",
        search.text_result
    );
    let fetch = call(
        &fx,
        "web_fetch",
        serde_json::json!({"url": "HTTPS://News.Example:443/story?id=7#top"}),
    )
    .await;
    assert!(
        !fetch.text_result.starts_with("tool_error"),
        "{}",
        fetch.text_result
    );
    assert_eq!(fx.calls("web_fetch"), 1);
}

#[tokio::test]
async fn a_constructed_url_is_not_in_context_and_the_broker_is_not_called() {
    let fx = fixture(Opts::default(), |_| Ok(ok(serde_json::json!({}), vec![]))).await;
    fx.prime("please read https://docs.example/guide");
    for url in [
        "https://docs.example/guide?leak=secret",
        "https://docs.example/guide/extra",
        "https://other.example/guide",
    ] {
        let resp = call(&fx, "web_fetch", serde_json::json!({ "url": url })).await;
        refused(&resp, "web_fetch", "not_in_context");
    }
    assert_eq!(fx.calls("web_fetch"), 0);
    // The prompt URL itself is fine.
    let resp = call(
        &fx,
        "web_fetch",
        serde_json::json!({"url": "https://docs.example/guide"}),
    )
    .await;
    assert!(
        !resp.text_result.starts_with("tool_error"),
        "{}",
        resp.text_result
    );
}

#[tokio::test]
async fn a_redirect_target_can_be_fetched_with_one_more_call() {
    let fx = fixture(Opts::default(), |req| {
        let url = req
            .args
            .as_ref()
            .and_then(|a| a.fields.get("url"))
            .and_then(|v| match v.kind.as_ref() {
                Some(prost_types::value::Kind::StringValue(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();
        if url.contains("old.example") {
            Ok(ok(
                serde_json::json!({"status": "redirected", "redirect_to": "https://new.example/home"}),
                vec![],
            ))
        } else {
            Ok(ok(serde_json::json!({"status": "ok"}), vec![]))
        }
    })
    .await;
    fx.prime("see https://old.example/");
    let first = call(
        &fx,
        "web_fetch",
        serde_json::json!({"url": "https://old.example/"}),
    )
    .await;
    assert!(first.text_result.contains("redirected"));
    let second = call(
        &fx,
        "web_fetch",
        serde_json::json!({"url": "https://new.example/home"}),
    )
    .await;
    assert!(
        !second.text_result.starts_with("tool_error"),
        "{}",
        second.text_result
    );
    assert_eq!(fx.calls("web_fetch"), 2);
}

#[tokio::test]
async fn an_internet_deny_rule_blocks_the_host() {
    let fx = fixture(
        Opts {
            deny: vec!["paste.example"],
            ..Default::default()
        },
        |_| Ok(ok(serde_json::json!({}), vec![])),
    )
    .await;
    fx.prime("https://paste.example/abc https://fine.example/abc");
    let resp = call(
        &fx,
        "web_fetch",
        serde_json::json!({"url": "https://paste.example/abc"}),
    )
    .await;
    refused(&resp, "web_fetch", "blocked");
    assert!(
        resp.text_result.contains("agent rule"),
        "{}",
        resp.text_result
    );
    assert!(resp.text_result.contains("paste.example"));
    assert_eq!(fx.calls("web_fetch"), 0);
    let fine = call(
        &fx,
        "web_fetch",
        serde_json::json!({"url": "https://fine.example/abc"}),
    )
    .await;
    assert!(
        !fine.text_result.starts_with("tool_error"),
        "{}",
        fine.text_result
    );
}

#[tokio::test]
async fn local_private_metadata_unresolvable_and_odd_port_targets_are_blocked() {
    let fx = fixture(Opts::default(), |_| Ok(ok(serde_json::json!({}), vec![]))).await;
    let urls = [
        "http://localhost/admin",
        "http://127.0.0.1/",
        "http://[::1]/",
        "http://10.1.2.3/x",
        "http://169.254.169.254/latest/meta-data/",
        "http://100.100.100.200/latest/meta-data/",
        "https://private.example/x",
        "https://imds.example/x",
        "https://nodns.example/x",
        "https://public.example:8080/x",
    ];
    fx.prime(&urls.join(" "));
    for url in urls {
        let resp = call(&fx, "web_fetch", serde_json::json!({ "url": url })).await;
        refused(&resp, "web_fetch", "blocked");
    }
    assert_eq!(fx.calls("web_fetch"), 0);
}

#[tokio::test]
async fn malformed_urls_are_invalid_url() {
    let fx = fixture(Opts::default(), |_| Ok(ok(serde_json::json!({}), vec![]))).await;
    for args in [
        serde_json::json!({}),
        serde_json::json!({"url": 7}),
        serde_json::json!({"url": "ftp://x.example/a"}),
        serde_json::json!({"url": "https://user:pw@x.example/a"}),
    ] {
        let resp = call(&fx, "web_fetch", args).await;
        refused(&resp, "web_fetch", "invalid_url");
    }
    assert_eq!(fx.calls("web_fetch"), 0);
}

#[tokio::test]
async fn a_search_query_with_a_url_is_invalid_and_a_bare_domain_passes() {
    let fx = fixture(Opts::default(), |_| {
        Ok(ok(serde_json::json!({"results": []}), vec![]))
    })
    .await;
    let bad = call(
        &fx,
        "web_search",
        serde_json::json!({"query": "see https://evil.example/exfil?data=abc"}),
    )
    .await;
    refused(&bad, "web_search", "invalid_query");
    assert!(bad.text_result.contains("web_fetch"));
    let bad_domains = call(
        &fx,
        "web_search",
        serde_json::json!({"query": "x", "include_domains": ["https://evil.example/p"]}),
    )
    .await;
    refused(&bad_domains, "web_search", "invalid_query");
    assert_eq!(fx.calls("web_search"), 0);
    let good = call(
        &fx,
        "web_search",
        serde_json::json!({"query": "pricing site example.com", "include_domains": ["example.com"]}),
    )
    .await;
    assert!(
        !good.text_result.starts_with("tool_error"),
        "{}",
        good.text_result
    );
    assert_eq!(fx.calls("web_search"), 1);
}

#[tokio::test]
async fn each_invoke_carries_only_the_secrets_its_export_lists() {
    let fx = fixture(Opts::default(), |_| Ok(ok(serde_json::json!({}), vec![]))).await;
    fx.prime("https://a.example/p");
    call(&fx, "web_search", serde_json::json!({"query": "q"})).await;
    call(
        &fx,
        "web_fetch",
        serde_json::json!({"url": "https://a.example/p"}),
    )
    .await;
    let seen = fx.seen.lock().unwrap().clone();
    let search = seen.iter().find(|s| s.tool == "web_search").unwrap();
    assert_eq!(search.secret_names, vec!["brave-api-key".to_string()]);
    assert_eq!(search.secret_values, vec![BRAVE.to_string()]);
    let fetch = seen.iter().find(|s| s.tool == "web_fetch").unwrap();
    assert!(fetch.secret_names.is_empty(), "{fetch:?}");
    assert!(fetch.secret_values.is_empty());
}

#[tokio::test]
async fn an_unbound_secret_of_another_tool_does_not_block_fetch() {
    let fx = fixture(
        Opts {
            put_other_secret: false,
            ..Default::default()
        },
        |_| Ok(ok(serde_json::json!({}), vec![])),
    )
    .await;
    fx.prime("https://a.example/p");
    let resp = call(
        &fx,
        "web_fetch",
        serde_json::json!({"url": "https://a.example/p"}),
    )
    .await;
    assert!(
        !resp.text_result.starts_with("tool_error"),
        "{}",
        resp.text_result
    );
}

#[tokio::test]
async fn a_broker_complaint_about_an_unlisted_secret_names_the_fix() {
    let fx = fixture(Opts::default(), |_| {
        Ok(InvokeResponse {
            result: Some(ToolResult {
                output: None,
                error: Some(vam_spec::v1::ErrorStatus {
                    code: 3,
                    message: "missing secret other-token".to_string(),
                    details: vec![],
                    retryable: false,
                }),
            }),
            ..Default::default()
        })
    })
    .await;
    let resp = call(&fx, "web_search", serde_json::json!({"query": "q"})).await;
    assert!(
        resp.text_result.starts_with(
            "tool_error: web_search: missing_secret: tool web_search does not declare secret other-token in required_secrets"
        ),
        "{}",
        resp.text_result
    );
    assert!(!resp.text_result.contains(OTHER));
    assert!(!resp.text_result.contains(BRAVE));
}

#[tokio::test]
async fn an_agent_without_web_gets_no_web_tools_and_no_skill() {
    for web in [Some(false), None] {
        let fx = fixture(
            Opts {
                web,
                ..Default::default()
            },
            |_| Ok(ok(serde_json::json!({}), vec![])),
        )
        .await;
        let catalog = fx.store.get_tool(SESSION, "web_fetch").unwrap();
        assert!(catalog.is_none(), "web={web:?}");
        assert!(fx.store.get_tool(SESSION, "web_search").unwrap().is_none());
        assert!(!fx.skills_dir.join(format!("hosted-{BROKER}")).exists());
    }
}

#[tokio::test]
async fn an_agent_with_web_gets_both_tools_preloaded() {
    let fx = fixture(Opts::default(), |_| Ok(ok(serde_json::json!({}), vec![]))).await;
    assert!(fx.store.get_tool(SESSION, "web_fetch").unwrap().is_some());
    assert!(fx.store.get_tool(SESSION, "web_search").unwrap().is_some());
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            fx.skills_dir
                .join(format!("hosted-{BROKER}"))
                .join("manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["preloaded"], true);
    assert_eq!(manifest["tools"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_forced_web_call_from_an_agent_with_web_off_is_refused() {
    let fx = fixture(
        Opts {
            web: Some(false),
            force_catalog: true,
            ..Default::default()
        },
        |_| Ok(ok(serde_json::json!({}), vec![])),
    )
    .await;
    fx.prime("https://a.example/p");
    let resp = call(
        &fx,
        "web_fetch",
        serde_json::json!({"url": "https://a.example/p"}),
    )
    .await;
    refused(&resp, "web_fetch", "web_disabled");
    let search = call(&fx, "web_search", serde_json::json!({"query": "q"})).await;
    refused(&search, "web_search", "web_disabled");
    assert!(fx.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_legacy_agent_or_a_missing_agent_never_has_web() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let _ = store.create_user("local", "local");
    store
        .create_session_with_owner(SESSION, None, "vm", "ws", "storage-default", "local")
        .unwrap();
    // No agent in the catalog.
    assert!(!session_web_enabled(&store, SESSION, Some(AGENT)));
    // An agent with no `permissions` at all.
    store
        .publish_agent_draft_with_owner(&agent(None, &[]), "local")
        .unwrap();
    store
        .release_agent(
            AGENT,
            &vam_spec::v1::Version {
                major: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!session_web_enabled(&store, SESSION, Some(AGENT)));
    assert!(!session_web_enabled(&store, "", Some(AGENT)));
}

#[tokio::test]
async fn a_public_web_preflight_without_a_session_is_refused() {
    use vam_orchestrator::hsp_registry::{HspRegistry, PreflightError};
    use vam_orchestrator::web_admission::AdmissionEnv;
    let store = SqliteWorkflowStore::in_memory().unwrap();
    store.upsert_hsp_broker_approval(&manifest()).unwrap();
    let request = InvokeRequest {
        tool_name: "web_fetch".to_string(),
        run_id: RUN.to_string(),
        args: Some(json_struct(
            serde_json::json!({"url": "https://a.example/p"}),
        )),
        ..Default::default()
    };
    let err = HspRegistry::new(&store)
        .preflight_invoke(
            BROKER,
            &HspEndpoint::Unix("/tmp/none.sock".into()),
            &request,
            &AdmissionEnv::none(),
        )
        .await
        .unwrap_err();
    match err {
        PreflightError::Refused(refusal) => assert_eq!(refusal.code, "web_disabled"),
        other => panic!("{other:?}"),
    }
}
