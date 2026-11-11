//! AR-1.8 / AR-1.10: a mock-model run against the REAL `vam.web` broker in process (mock provider,
//! fixture fetcher) through the host-tool path, then the finish check. Proves that
//! `finalOutputs.sources` holds cited sources only and no snippet, that unresolved ids are flagged, and
//! that the search key (a canary) never reaches anything the guest receives or the outbox.
//! DNS is injected; nothing touches the network and no test changes process environment.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use prost_types::Struct;
use tempfile::tempdir;
use vam_inference::SessionBudget;
use vam_orchestrator::host_tools::HostToolRegistry;
use vam_orchestrator::hsp::HspEndpoint;
use vam_orchestrator::hsp_store::{seed_hsp_skills, HspBrokerToolSnapshot};
use vam_orchestrator::secrets::{SecretKind, SecretsVault};
use vam_orchestrator::security::SessionSecurityContext;
use vam_orchestrator::session::{builtin_tool_catalog, merge_tool_catalog};
use vam_orchestrator::web_admission::session_web_enabled;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_security::ssrf_guard::HostResolver;
use vam_security::PolicyMode;
use vam_spec::v1::{
    AgentPermissions, AgentSpec, ExecuteHostToolRequest, ExecuteHostToolResponse, ExecutionLease,
    HostDefault, InternetPermissions, PutHostedBrokerRequest, ResourceId, ToolIntentRequest,
    WebPermissions,
};
use vam_spec::validation::host_tool_digest;
use vam_web::fetch::FixtureFetcher;
use vam_web::provider::mock::MockProvider;
use vam_web::WebBroker;

const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-web-sources";
const AGENT: &str = "urn:vam:agent:local.vam.dev/acme/web-sources";
const SESSION: &str = "sess-web-sources";
const BROKER: &str = "vam.web";
const CANARY: &str = "BENCH_SECRET_CANARY_hermetic_0123456789";
const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/bench/fixtures/web"
);
const MANIFEST: &str = include_str!("../../../scripts/web-broker/manifest.json");

struct PublicDns;
impl HostResolver for PublicDns {
    fn lookup(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        let ip: IpAddr = "93.184.216.34".parse().unwrap();
        Ok(vec![SocketAddr::new(ip, port)])
    }
}

fn json_struct(value: serde_json::Value) -> Struct {
    vam_spec::encoding::json_to_proto::<Struct>("google.protobuf.Struct", &value.to_string())
        .unwrap()
}

fn agent(web: bool) -> AgentSpec {
    AgentSpec {
        resource_id: Some(ResourceId {
            urn: AGENT.to_string(),
        }),
        display_name: "web".to_string(),
        description: "web sources".to_string(),
        role: "assistant".to_string(),
        instruction_source: Some(
            vam_spec::v1::agent_spec::InstructionSource::SystemInstructions("x".to_string()),
        ),
        permissions: Some(AgentPermissions {
            tools: None,
            internet: Some(InternetPermissions {
                default: HostDefault::Allow as i32,
                rules: vec![],
            }),
            advanced: vec![],
            web: Some(WebPermissions { enabled: web }),
        }),
        ..Default::default()
    }
}

struct Fx {
    store: Arc<SqliteWorkflowStore>,
    vault: Arc<SecretsVault>,
    ctx: SessionSecurityContext,
    registry: HostToolRegistry,
    skills_dir: std::path::PathBuf,
    catalog_text: String,
    _dir: tempfile::TempDir,
}

async fn fixture(web: bool) -> Fx {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let request: PutHostedBrokerRequest =
        vam_spec::encoding::json_to_proto("vam.v1.PutHostedBrokerRequest", MANIFEST).unwrap();
    let manifest = request.manifest.unwrap();
    assert_eq!(manifest.broker_id, BROKER);
    store.upsert_hsp_broker_approval(&manifest).unwrap();

    // The key is stored the way run-web-broker.sh stores it: kind inference, org scope.
    let vault = SecretsVault::for_tests(store.clone());
    vault
        .put_secret(
            "web/brave-api-key",
            CANARY,
            SecretKind::Inference,
            &[],
            None,
        )
        .unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("web.sock");
    let listener = vam_web::serve::bind_unix_listener(&sock).unwrap();
    let broker = WebBroker::new(
        Arc::new(MockProvider::new(FIXTURES.into())),
        Arc::new(FixtureFetcher::new(FIXTURES.into())),
    );
    tokio::spawn(async move {
        let _ = vam_web::serve::serve_unix(listener, broker).await;
    });
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
        .publish_agent_draft_with_owner(&agent(web), "local")
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
    let web_enabled = session_web_enabled(&store, SESSION, Some(AGENT));
    seed_hsp_skills(&skills_dir, store.as_ref(), web_enabled).unwrap();
    let skills: Vec<_> = vam_orchestrator::session::parse_skills_catalog(&skills_dir)
        .into_iter()
        .filter(|entry| !entry.entrypoint.starts_with("hosted:"))
        .collect();
    let hosted = store.list_hsp_catalog_entries(web_enabled).unwrap();
    let catalog = merge_tool_catalog(builtin_tool_catalog(), skills, hosted).unwrap();
    store.record_tool_catalog(SESSION, &catalog).unwrap();
    let catalog_text = format!("{:?}", store.get_tool_catalog(SESSION).unwrap());

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
    .with_web_resolver(Arc::new(PublicDns));
    Fx {
        store,
        vault,
        ctx,
        registry: HostToolRegistry::with_test_adapters(),
        skills_dir,
        catalog_text,
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

/// Search, then fetch the first two hits, as the mock model of the U2 bench does. Returns every
/// response the guest would receive.
async fn research(fx: &Fx) -> Vec<ExecuteHostToolResponse> {
    let mut out = Vec::new();
    out.push(
        call(
            fx,
            "web_search",
            serde_json::json!({"query": "rust 1.99 changes"}),
        )
        .await,
    );
    out.push(
        call(
            fx,
            "web_fetch",
            serde_json::json!({"url": "https://blog.example.org/rust-1-99"}),
        )
        .await,
    );
    out.push(
        call(
            fx,
            "web_fetch",
            serde_json::json!({"url": "https://news.example.net/rust-1-99-changes"}),
        )
        .await,
    );
    for resp in &out {
        assert!(
            !resp.text_result.starts_with("tool_error"),
            "{}",
            resp.text_result
        );
    }
    out
}

fn start_run(fx: &Fx) {
    fx.store
        .persist_run_spec(RUN, "{}", Some(SESSION), AGENT, None, "local")
        .unwrap();
}

#[tokio::test]
async fn cited_answer_gets_sources_in_order_without_snippets_and_unresolved_is_flagged() {
    let fx = fixture(true).await;
    start_run(&fx);
    let responses = research(&fx).await;

    // The guest does see snippets in the search result: that is the conversation, not the host.
    assert!(responses[0].text_result.contains("faster builds"));

    let answer = "Faster builds [S2] and two APIs [S1, S3] but also [S9].";
    let citations =
        vam_orchestrator::api::web_citations_for_answer(&fx.store, RUN, answer).expect("web run");
    let result: serde_json::Value = serde_json::from_str(
        &vam_orchestrator::api::answer_result_json(answer, Some(true), Some(&citations)),
    )
    .unwrap();

    assert_eq!(result["answer"], answer, "the answer text is never changed");
    let ids: Vec<&str> = result["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["S2", "S1", "S3"], "order of first citation");
    assert_eq!(result["citations"]["unresolved"], serde_json::json!(["S9"]));
    let first = &result["sources"][0];
    assert_eq!(first["url"], "https://news.example.net/rust-1-99-changes");
    assert_eq!(first["kind"], "page", "fetched sources are pages");
    assert!(first["artifactPath"].is_string());
    let rendered = result.to_string();
    for snippet in ["faster builds", "A walkthrough", "Users discuss"] {
        assert!(
            !rendered.contains(snippet),
            "no snippet in the result: {snippet}"
        );
    }
    assert!(rendered.find("\"snippet\"").is_none());

    let events = fx.store.read_outbox(0).unwrap();
    let flagged: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "vam.v1.web.citations_unresolved")
        .collect();
    assert_eq!(flagged.len(), 1);
    let payload: serde_json::Value = serde_json::from_str(&flagged[0].payload).unwrap();
    assert_eq!(payload["unresolved"], serde_json::json!(["S9"]));
    assert_eq!(payload["resolved"], 3);
}

#[tokio::test]
async fn an_answer_without_markers_has_no_sources_key() {
    let fx = fixture(true).await;
    start_run(&fx);
    research(&fx).await;
    assert!(
        vam_orchestrator::api::web_citations_for_answer(&fx.store, RUN, "Plain answer.").is_none()
    );
    let result: serde_json::Value = serde_json::from_str(
        &vam_orchestrator::api::answer_result_json("Plain answer.", Some(true), None),
    )
    .unwrap();
    assert!(result.get("sources").is_none() && result.get("citations").is_none());
}

#[tokio::test]
async fn markers_with_no_recorded_sources_are_all_unresolved_and_no_web_agent_gets_no_keys() {
    let fx = fixture(true).await;
    start_run(&fx);
    let c = vam_orchestrator::api::web_citations_for_answer(&fx.store, RUN, "Made up [S1].")
        .expect("web agent");
    assert!(c.sources.is_empty());
    assert_eq!(c.unresolved, ["S1"]);

    let off = fixture(false).await;
    start_run(&off);
    assert!(
        vam_orchestrator::api::web_citations_for_answer(&off.store, RUN, "Cites [S1] anyway.")
            .is_none(),
        "an agent without web never gets sources or citations keys"
    );
}

#[tokio::test]
async fn the_search_key_never_reaches_the_guest_the_catalog_the_skills_or_the_outbox() {
    let fx = fixture(true).await;
    start_run(&fx);
    let responses = research(&fx).await;
    let answer = "Cited [S1] and [S9].";
    let citations =
        vam_orchestrator::api::web_citations_for_answer(&fx.store, RUN, answer).unwrap();
    let result = vam_orchestrator::api::answer_result_json(answer, Some(true), Some(&citations));

    for (i, resp) in responses.iter().enumerate() {
        assert!(
            !format!("{resp:?}").contains(CANARY),
            "ExecuteHostToolResponse {i} leaked the key"
        );
    }
    assert!(
        !fx.catalog_text.contains(CANARY),
        "the catalog leaked the key"
    );
    assert!(!result.contains(CANARY), "the final outputs leaked the key");
    for event in fx.store.read_outbox(0).unwrap() {
        assert!(
            !event.payload.contains(CANARY),
            "outbox event {} leaked the key",
            event.event_type
        );
    }
    let mut stack = vec![fx.skills_dir.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                assert!(!text.contains(CANARY), "{} leaked the key", path.display());
            }
        }
    }

    // The key is a vault row of kind inference: the guest egress proxy never gets a binding for it.
    let bindings = fx.vault.session_egress_bindings().unwrap();
    assert!(
        bindings.iter().all(|b| b.binding_id != "web/brave-api-key"),
        "the web key must not be a session egress binding"
    );
    assert!(!format!("{bindings:?}").contains(CANARY));
}
