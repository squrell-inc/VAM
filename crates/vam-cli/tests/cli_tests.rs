use std::process::Command;

use clap::Parser;
use httpmock::Method::PUT;
use httpmock::MockServer;
use vam_cli::client::Client;
use vam_cli::config::ResolvedConfig;

/// `vam_cli::output` keeps `--json` mode in a process-wide flag, and the
/// `--json` tests switch it on. Every test holds this lock and starts from
/// human mode, so one test's flag never changes another test's output path.
static PROCESS_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn node_status_sends_bearer_token() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/node")
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"status":{"nodeId":"local","ready":true}}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cfg = ResolvedConfig {
            host: server.base_url(),
            api_key: "vam_sk_test".into(),
        };
        let client = Client::new(&cfg).unwrap();
        let body = client.get("/v1/node").await.unwrap();
        assert!(body.contains("nodeId"));
    });

    mock.assert();
}

#[test]
fn secrets_set_sends_auth_and_idempotency_without_logging_secret() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(PUT)
            .path("/v1/secrets")
            .header("authorization", "Bearer vam_sk_test")
            .header_exists("Idempotency-Key");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"secret":{"bindingId":"openai/api-key","fingerprint":"abc12345","kind":1}}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cfg = ResolvedConfig {
            host: server.base_url(),
            api_key: "vam_sk_test".into(),
        };
        let client = Client::new(&cfg).unwrap();
        let req = vam_spec::v1::PutHostSecretRequest {
            binding_id: "openai/api-key".into(),
            value: "sk-live-secret".into(),
            kind: 1,
            ..Default::default()
        };
        let body = client
            .put_json("/v1/secrets", "vam.v1.PutHostSecretRequest", &req, None)
            .await
            .unwrap();
        assert!(!body.contains("sk-live-secret"));
        assert!(body.contains("fingerprint"));
    });

    mock.assert();
}

#[test]
fn run_start_409_body_surfaces_message() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/runs")
            .header("authorization", "Bearer vam_sk_test")
            .header("Idempotency-Key", "test-idem");
        then.status(409)
            .header("content-type", "application/json")
            .body(r#"{"code":"FailedPrecondition","message":"agent is running on workspace 'assistant' (session sess-1, run run-1)","retryable":true}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(async {
        let cfg = ResolvedConfig {
            host: server.base_url(),
            api_key: "vam_sk_test".into(),
        };
        let client = Client::new(&cfg).unwrap();
        let req = vam_spec::v1::CreateRunRequest {
            spec: Some(vam_spec::v1::RunSpec {
                workspace_ref_uri: "volume://assistant".into(),
                idempotency_key: "test-idem".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        client
            .post_json(
                "/v1/runs",
                "vam.v1.CreateRunRequest",
                &req,
                Some("test-idem"),
            )
            .await
    });

    let err = result.expect_err("409 is an error");
    assert_eq!(
        vam_cli::output::classify(&err),
        (vam_cli::output::ExitCode::Conflict, Some(409))
    );
    let text = err.to_string();
    assert!(!text.contains("HTTP"), "{text}");
    assert!(!text.contains("409"), "{text}");
    assert!(
        text.contains("Wrong: agent is running on workspace 'assistant'"),
        "{text}"
    );
    assert!(
        text.contains("Expected: no active run on that workspace."),
        "{text}"
    );
    assert!(text.contains("Fix: vam run cancel run-1."), "{text}");
    mock.assert();
}

#[test]
fn dry_run_prints_decisions_and_does_not_post_a_run() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let agent = "urn:vam:agent:examples.local/vam/demo";
    let policy = "urn:vam:policy:examples.local/vam/demo";
    let skill = "urn:vam:skill:examples.local/vam/demo-tools";
    install_catalog(&server, agent, policy, skill, None);

    let post = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/v1/runs");
        then.status(500).body("should not start");
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(async {
        let args = vam_cli::cmd::run::RunArgs::try_parse_from(["vam", agent, "--dry-run"]).unwrap();
        vam_cli::cmd::run::run(Some(server.base_url()), Some("vam_sk_test".into()), args).await
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(post.hits(), 0);
}

#[test]
fn playbook_inputs_are_posted_and_invalid_inputs_start_nothing() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let agent = "urn:vam:agent:examples.local/vam/demo";
    let policy = "urn:vam:policy:examples.local/vam/demo";
    let skill = "urn:vam:skill:examples.local/vam/demo-tools";
    let playbook = "urn:vam:playbook:examples.local/vam/demo";
    install_catalog(&server, agent, policy, skill, Some(playbook));

    let post = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/runs")
            .body_contains(playbook)
            .body_contains("cutoff_days")
            .body_contains("PROJ");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"runId":{"urn":"urn:vam:run:local/demo"},"status":{"runId":{"urn":"urn:vam:run:local/demo"}}}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let ok = rt.block_on(async {
        let args = vam_cli::cmd::run::RunArgs::try_parse_from([
            "vam",
            agent,
            "--playbook",
            playbook,
            "--input",
            "project_key=PROJ",
            "--input",
            "cutoff_days=14",
        ])
        .unwrap();
        vam_cli::cmd::run::run(Some(server.base_url()), Some("vam_sk_test".into()), args).await
    });
    assert!(ok.is_ok(), "{ok:?}");
    post.assert();

    let rejected = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/v1/runs");
        then.status(500).body("should not start");
    });
    let bad = rt.block_on(async {
        let args = vam_cli::cmd::run::RunArgs::try_parse_from([
            "vam",
            agent,
            "--playbook",
            playbook,
            "--input",
            "project_key=PROJ",
            "--input",
            "cutoff_days=nope",
        ])
        .unwrap();
        vam_cli::cmd::run::run(Some(server.base_url()), Some("vam_sk_test".into()), args).await
    });
    let err = bad.expect_err("bad integer").to_string();
    assert!(err.contains("cutoff_days"), "{err}");
    assert_eq!(rejected.hits(), 0);
}

fn install_catalog(
    server: &httpmock::MockServer,
    agent: &str,
    policy: &str,
    skill: &str,
    playbook: Option<&str>,
) {
    let agent_body = catalog_agent_json(agent, policy, skill);
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path(format!(
            "/v1/agents/{}",
            vam_cli::client::encode_path_segment(agent)
        ));
        then.status(200).body(agent_body);
    });
    let policy_body = catalog_policy_json(policy);
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path(format!(
            "/v1/policies/{}",
            vam_cli::client::encode_path_segment(policy)
        ));
        then.status(200).body(policy_body);
    });
    let skill_body = catalog_skill_json(skill);
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path(format!(
            "/v1/skills/{}",
            vam_cli::client::encode_path_segment(skill)
        ));
        then.status(200).body(skill_body);
    });
    if let Some(playbook) = playbook {
        let body = catalog_playbook_json(playbook);
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path(format!(
                "/v1/playbooks/{}",
                vam_cli::client::encode_path_segment(playbook)
            ));
            then.status(200).body(body);
        });
    }
}

fn catalog_agent_json(agent: &str, policy: &str, skill: &str) -> String {
    let spec = vam_spec::v1::AgentSpec {
        resource_id: Some(vam_spec::v1::ResourceId {
            urn: agent.to_string(),
        }),
        display_name: "Demo".into(),
        role: "assistant".into(),
        allow_any_playbook: true,
        capability_policy: Some(vam_spec::v1::CapabilityPolicyRef {
            policy_ref: Some(vam_spec::v1::ResourceRef {
                resource_id: Some(vam_spec::v1::ResourceId {
                    urn: policy.to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        skill_refs: vec![vam_spec::v1::ResourceRef {
            resource_id: Some(vam_spec::v1::ResourceId {
                urn: skill.to_string(),
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    let resp = vam_spec::v1::GetAgentResponse {
        spec: Some(spec),
        channel: 2,
        ..Default::default()
    };
    vam_spec::encoding::proto_to_json("vam.v1.GetAgentResponse", &resp).unwrap()
}

fn catalog_policy_json(policy: &str) -> String {
    let cedar = r#"
permit (
    principal == Agent::"urn:vam:agent:examples.local/vam/demo",
    action == Action::"ExecuteTool",
    resource == Tool::"read_file"
);
permit (
    principal == Agent::"urn:vam:agent:examples.local/vam/demo",
    action == Action::"ExecuteTool",
    resource == Tool::"echo_ping"
)
when { context has hitl_approved && context.hitl_approved == true };
"#
    .to_string();
    let resp = vam_spec::v1::GetPolicyResponse {
        record: Some(vam_spec::v1::PolicyRecord {
            resource_id: Some(vam_spec::v1::ResourceId {
                urn: policy.to_string(),
            }),
            cedar,
            ..Default::default()
        }),
    };
    vam_spec::encoding::proto_to_json("vam.v1.GetPolicyResponse", &resp).unwrap()
}

fn catalog_skill_json(skill: &str) -> String {
    let resp = vam_spec::v1::GetSkillResponse {
        spec: Some(vam_spec::v1::SkillManifest {
            resource_id: Some(vam_spec::v1::ResourceId {
                urn: skill.to_string(),
            }),
            tools: vec![vam_spec::v1::ToolSpec {
                name: "echo_ping".into(),
                description: "ping".into(),
                input_schema_json: "{}".into(),
                side_effect: 1,
                ..Default::default()
            }],
            ..Default::default()
        }),
    };
    vam_spec::encoding::proto_to_json("vam.v1.GetSkillResponse", &resp).unwrap()
}

fn catalog_playbook_json(playbook: &str) -> String {
    let spec: vam_spec::v1::PlaybookSpec = vam_spec::encoding::json_to_proto(
        "vam.v1.PlaybookSpec",
        &format!(
            r#"{{
                "resourceId": {{"urn": "{playbook}"}},
                "displayName": "Demo",
                "steps": [{{
                    "id": "search",
                    "instruction": "Search.",
                    "mode": "STEP_MODE_AGENTIC"
                }}],
                "inputSchema": {{
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["project_key", "cutoff_days"],
                    "properties": {{
                        "project_key": {{"type": "string"}},
                        "cutoff_days": {{"type": "integer"}}
                    }}
                }}
            }}"#
        ),
    )
    .unwrap();
    let resp = vam_spec::v1::GetPlaybookResponse { spec: Some(spec) };
    vam_spec::encoding::proto_to_json("vam.v1.GetPlaybookResponse", &resp).unwrap()
}

#[test]
fn encode_path_segment_escapes_slashes_in_urns() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let encoded = vam_cli::client::encode_path_segment("urn:vam:run:local.vam.dev/acme/run-test");
    assert_eq!(encoded, "urn%3Avam%3Arun%3Alocal.vam.dev%2Facme%2Frun-test");
    assert!(!encoded.contains('/'));
}

#[test]
fn decode_path_segment_reverses_encode() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let urn = "urn:vam:hitl:local.vam.dev/acme/req-1787343348-1";
    let encoded = vam_cli::client::encode_path_segment(urn);
    let decoded = vam_cli::client::decode_path_segment(&encoded).unwrap();
    assert_eq!(decoded, urn);
}

#[test]
fn normalize_resource_id_accepts_raw_or_encoded_urns() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let urn = "urn:vam:hitl:local.vam.dev/acme/req-1787343348-1";
    let encoded = vam_cli::client::encode_path_segment(urn);
    assert_eq!(vam_cli::client::normalize_resource_id(urn).unwrap(), urn);
    assert_eq!(
        vam_cli::client::normalize_resource_id(&encoded).unwrap(),
        urn
    );
}

#[test]
fn approve_get_encodes_urn_path_segment() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let approval_urn = "urn:vam:hitl:local.vam.dev/acme/req-1787343348-1";
    let encoded = vam_cli::client::encode_path_segment(approval_urn);

    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/approvals/{encoded}"))
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"approval":{"claims":{"requestId":{"urn":"urn:vam:hitl:local.vam.dev/acme/req-1787343348-1"}}},"status":{"state":"APPROVAL_STATE_PENDING"}}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::approve::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::approve::ApproveCmd::Get {
                id: approval_urn.to_string(),
            },
        )
        .await
        .unwrap();
    });

    mock.assert();
}

#[test]
fn approve_ls_lists_parked_runs() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let body = r#"{"approvals":[{"claims":{"requestId":{"urn":"urn:vam:hitl:local.vam.dev/acme/approval-parked"},"runId":{"urn":"urn:vam:run:local.vam.dev/acme/run-parked"},"targetToolName":"jira_create_issue"}}]}"#;
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/approvals")
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(body);
    });

    let listed = vam_cli::cmd::approve::format_parked_runs(body);
    assert!(listed.contains("run=urn:vam:run:local.vam.dev/acme/run-parked"));
    assert!(listed.contains("tool=jira_create_issue"));
    assert!(listed.contains("approval=urn:vam:hitl:local.vam.dev/acme/approval-parked"));

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::approve::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::approve::ApproveCmd::Ls { all: false },
        )
        .await
        .unwrap();
    });
    mock.assert();
}

#[test]
fn list_paged_follows_the_token_only_with_all() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(true);
    let server = MockServer::start();
    let first = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/approvals")
            .matches(|req| {
                !req.query_params
                    .as_ref()
                    .is_some_and(|q| q.iter().any(|(k, _)| k == "pageToken"))
            });
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"approvals":[{"n":1},{"n":2}],"pagination":{"nextPageToken":"tok-2"}}"#);
    });
    let second = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/approvals")
            .query_param("pageToken", "tok-2");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"approvals":[{"n":3}],"pagination":{"nextPageToken":""}}"#);
    });
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cfg = vam_cli::config::resolve(Some(server.base_url()), Some("vam_sk_test".into()))
            .await
            .unwrap();
        let c = vam_cli::client::Client::new(&cfg).unwrap();
        // One page by default: the second page is not asked for.
        let one = vam_cli::client::list_paged(&c, "/v1/approvals", "approvals", false)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&one).unwrap();
        assert_eq!(v["approvals"].as_array().unwrap().len(), 2);
        assert_eq!(second.hits(), 0);
        // `--all` follows it and merges.
        let every = vam_cli::client::list_paged(&c, "/v1/approvals", "approvals", true)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&every).unwrap();
        assert_eq!(v["approvals"].as_array().unwrap().len(), 3);
        assert!(v.get("pagination").is_none());
    });
    assert_eq!(first.hits(), 2);
    assert_eq!(second.hits(), 1);
    vam_cli::output::set_json(false);
}

#[test]
fn approve_decide_parked_tool_hitl() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let approval_urn = "urn:vam:hitl:local.vam.dev/acme/approval-parked";
    let encoded = vam_cli::client::encode_path_segment(approval_urn);

    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path(format!("/v1/approvals/{encoded}/decide"))
            .header("authorization", "Bearer vam_sk_test")
            .header_exists("Idempotency-Key")
            .body_contains(approval_urn)
            .body_contains("HITL_DECISION_APPROVED");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"status":{"state":"APPROVAL_STATE_APPROVED"}}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::approve::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::approve::ApproveCmd::Decide {
                id: approval_urn.to_string(),
                decision: "approved".to_string(),
                reason: None,
                amended_args: None,
                response: None,
                expected_revision: None,
                always: None,
                idempotency_key: Some("parked-decide".into()),
            },
        )
        .await
        .unwrap();
    });

    mock.assert();
}

#[test]
fn approve_decide_uses_encoded_path_and_canonical_body_id() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let approval_urn = "urn:vam:hitl:local.vam.dev/acme/req-1787343348-1";
    let encoded = vam_cli::client::encode_path_segment(approval_urn);

    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path(format!("/v1/approvals/{encoded}/decide"))
            .header("authorization", "Bearer vam_sk_test")
            .header_exists("Idempotency-Key");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"status":{"state":"APPROVAL_STATE_APPROVED"}}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::approve::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::approve::ApproveCmd::Decide {
                id: approval_urn.to_string(),
                decision: "approved".to_string(),
                reason: None,
                amended_args: None,
                response: None,
                expected_revision: None,
                always: None,
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    });

    mock.assert();
}

#[test]
fn approve_decide_posts_a_submitted_gate_response() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let approval_urn = "urn:vam:hitl:local.vam.dev/acme/gate-approve";
    let encoded = vam_cli::client::encode_path_segment(approval_urn);

    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path(format!("/v1/approvals/{encoded}/decide"))
            .header("authorization", "Bearer vam_sk_test")
            .header_exists("Idempotency-Key")
            .body_contains("HITL_DECISION_SUBMITTED")
            .body_contains("looks good");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"status":{"state":"APPROVAL_STATE_SUBMITTED"}}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::approve::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::approve::ApproveCmd::Decide {
                id: approval_urn.to_string(),
                decision: "submitted".to_string(),
                reason: None,
                amended_args: None,
                response: Some(r#"{"comment":"looks good"}"#.to_string()),
                expected_revision: None,
                always: None,
                idempotency_key: Some("gate-decide".into()),
            },
        )
        .await
        .unwrap();
    });

    mock.assert();
}

#[test]
fn follow_run_events_terminates_on_completed_event() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let run_urn = "urn:vam:run:local.vam.dev/acme/run-follow-test";
    let encoded = vam_cli::client::encode_path_segment(run_urn);

    let events_mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{}/events", encoded))
            .query_param("after", "0")
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"events":[{"sequence":"1","eventType":"vam.v1.run.completed","payload":"{\"answer\":\"42\"}"}]}"#);
    });

    let status_mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{}", encoded))
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"status":{"state":"RUN_STATE_SUCCEEDED","finalOutputs":{"answer":"42"}}}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cfg = ResolvedConfig {
            host: server.base_url(),
            api_key: "vam_sk_test".into(),
        };
        let client = Client::new(&cfg).unwrap();
        let res = vam_cli::cmd::run::follow_run_events(&client, run_urn, 0).await;
        assert!(res.is_ok());
    });

    events_mock.assert();
    status_mock.assert();
}

#[test]
fn follow_run_events_terminates_on_failed_event() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let run_urn = "urn:vam:run:local.vam.dev/acme/run-failed-test";
    let encoded = vam_cli::client::encode_path_segment(run_urn);

    let events_mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{}/events", encoded))
            .query_param("after", "0")
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"events":[{"sequence":"1","eventType":"vam.v1.run.failed","payload":"{\"error\":\"task timed out\"}"}]}"#);
    });

    let status_mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{}", encoded))
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{"status":{"state":"RUN_STATE_FAILED","error":{"message":"task timed out"}}}"#,
            );
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cfg = ResolvedConfig {
            host: server.base_url(),
            api_key: "vam_sk_test".into(),
        };
        let client = Client::new(&cfg).unwrap();
        let res = vam_cli::cmd::run::follow_run_events(&client, run_urn, 0).await;
        assert!(res.is_ok());
    });

    events_mock.assert();
    status_mock.assert();
}

#[test]
fn follow_run_events_terminates_when_same_completed_event_redelivered() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let run_urn = "urn:vam:run:local.vam.dev/acme/run-redeliver-test";
    let encoded = vam_cli::client::encode_path_segment(run_urn);

    // First poll: after=0 returns completed. Second poll: after=92 still returns the same
    // event (simulates server not honoring the cursor). Follow must still exit.
    let events_mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{}/events", encoded))
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"events":[{"sequence":"92","eventType":"vam.v1.run.completed","payload":"{\"answer\":\"done\"}"}]}"#);
    });

    let status_mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{}", encoded))
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"status":{"state":"RUN_STATE_RUNNING"}}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cfg = ResolvedConfig {
            host: server.base_url(),
            api_key: "vam_sk_test".into(),
        };
        let client = Client::new(&cfg).unwrap();
        let res = vam_cli::cmd::run::follow_run_events(&client, run_urn, 0).await;
        assert!(res.is_ok());
    });

    events_mock.assert();
    status_mock.assert();
}

#[test]
fn follow_run_events_terminates_on_status_alone_if_no_events() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let run_urn = "urn:vam:run:local.vam.dev/acme/run-status-test";
    let encoded = vam_cli::client::encode_path_segment(run_urn);

    let events_mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{}/events", encoded))
            .query_param("after", "0")
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"events":[]}"#);
    });

    let status_mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{}", encoded))
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{"status":{"state":"RUN_STATE_SUCCEEDED","finalOutputs":{"answer":"hello"}}}"#,
            );
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cfg = ResolvedConfig {
            host: server.base_url(),
            api_key: "vam_sk_test".into(),
        };
        let client = Client::new(&cfg).unwrap();
        let res = vam_cli::cmd::run::follow_run_events(&client, run_urn, 0).await;
        assert!(res.is_ok());
    });

    events_mock.assert();
    status_mock.assert();
}

#[test]
fn follow_run_events_accepts_playbook_step_statuses() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let run_urn = "urn:vam:run:local.vam.dev/acme/run-step-progress";
    let encoded = vam_cli::client::encode_path_segment(run_urn);

    let events_mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{}/events", encoded))
            .query_param("after", "0")
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"events":[]}"#);
    });

    let status_mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{}", encoded))
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{"status":{"state":"RUN_STATE_SUCCEEDED","finalOutputs":{"answer":"ok"},"stepStatuses":[{"stepId":"query","state":"STEP_STATE_SUCCESS","outcome":"done","summary":"found issues","transitionRevision":"2","evidenceRefs":[]}]}}"#,
            );
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cfg = ResolvedConfig {
            host: server.base_url(),
            api_key: "vam_sk_test".into(),
        };
        let client = Client::new(&cfg).unwrap();
        let res = vam_cli::cmd::run::follow_run_events(&client, run_urn, 0).await;
        assert!(res.is_ok());
    });

    events_mock.assert();
    status_mock.assert();
}

#[test]
fn skill_publish_sends_bearer_idempotency_and_multipart() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::{copy, Cursor};

    fn minimal_archive() -> Vec<u8> {
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let skills = b"---\nid: \"urn:vam:skill:vam.local/vam/test\"\nversion: \"1.0.0\"\n---\n\n# Body\n";
            let manifest = br#"{"resourceId":{"urn":"urn:vam:skill:vam.local/vam/test"},"version":{"major":1,"minor":0,"patch":0},"description":"t","tools":[]}"#;
            for (path, bytes) in [
                ("SKILLS.md", skills.as_slice()),
                ("manifest.json", manifest.as_slice()),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, path, &mut Cursor::new(bytes))
                    .expect("tar append");
            }
            builder.finish().expect("tar finish");
        }
        let mut gz = Vec::new();
        {
            let mut encoder = GzEncoder::new(&mut gz, Compression::default());
            copy(&mut Cursor::new(&tar_buf), &mut encoder).expect("gzip");
            encoder.finish().expect("gzip finish");
        }
        gz
    }

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/skills/packages")
            .header("authorization", "Bearer vam_sk_test")
            .header_exists("Idempotency-Key")
            .header_exists("content-type");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"package":{"digest":"abc","channel":"DRAFT"}}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cfg = ResolvedConfig {
            host: server.base_url(),
            api_key: "vam_sk_test".into(),
        };
        let client = Client::new(&cfg).unwrap();
        let body = client
            .post_multipart(
                "/v1/skills/packages",
                "file",
                "skill-package.tar.gz",
                "application/gzip",
                minimal_archive(),
                None,
            )
            .await
            .unwrap();
        assert!(body.contains("DRAFT"));
    });

    mock.assert();
}

#[test]
fn receipt_show_and_the_evidence_alias_read_the_run_receipt_endpoint() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let run_urn = "urn:vam:run:local.vam.dev/acme/receipt-cli";
    let encoded = vam_cli::client::encode_path_segment(run_urn);
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{encoded}/receipt"))
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"receipt":{"runId":"r"},"modelAppendix":{},"signature":{}}"#);
    });
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::receipt::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::receipt::ReceiptCmd::Show {
                id: run_urn.to_string(),
            },
        )
        .await
        .unwrap();
        vam_cli::cmd::evidence::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::evidence::EvidenceCmd::Receipt {
                id: run_urn.to_string(),
            },
        )
        .await
        .unwrap();
    });
    mock.assert_hits(2);
}

#[test]
fn evidence_export_writes_directory() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let run_urn = "urn:vam:run:local.vam.dev/acme/evidence-cli";
    let encoded = vam_cli::client::encode_path_segment(run_urn);
    let pack = serde_json::json!({
        "runId": run_urn,
        "exportedAtUnixMs": 1778000000000u64,
        "files": {
            "manifest.json": "{\"runId\":\"urn:vam:run:local.vam.dev/acme/evidence-cli\"}",
            "attestation.json": "{\"run_id\":\"urn:vam:run:local.vam.dev/acme/evidence-cli\"}",
            "attestation.dsse.json": "{\"payloadType\":\"application/vnd.vam.run-outcome+json\"}",
            "audit.jsonl": "",
            "trajectory.json": "{\"digest\":\"d1\",\"events\":[]}",
            "artifacts.json": "{\"artifacts\":[],\"paths\":[]}",
            "denials.jsonl": ""
        }
    });
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{encoded}/evidence"))
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(pack.to_string());
    });

    let out = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::evidence::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::evidence::EvidenceCmd::Export {
                id: run_urn.to_string(),
                out: out.path().to_path_buf(),
            },
        )
        .await
        .unwrap();
    });

    mock.assert();
    assert!(out.path().join("manifest.json").is_file());
    assert!(out.path().join("attestation.json").is_file());
    assert!(out.path().join("attestation.dsse.json").is_file());
    assert!(out.path().join("audit.jsonl").is_file());
    assert!(out.path().join("trajectory.json").is_file());
    assert!(out.path().join("artifacts.json").is_file());
    assert!(out.path().join("denials.jsonl").is_file());
}

#[test]
fn evidence_export_two_users_alice_cannot_pull_bob() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let bob_run = "urn:vam:run:local.vam.dev/acme/evidence-bob";
    let encoded = vam_cli::client::encode_path_segment(bob_run);
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{encoded}/evidence"))
            .header("authorization", "Bearer vam_sk_alice");
        then.status(404)
            .header("content-type", "application/json")
            .body(r#"{"code":"NotFound","message":"run not found"}"#);
    });

    let out = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let err = rt.block_on(async {
        vam_cli::cmd::evidence::run(
            Some(server.base_url()),
            Some("vam_sk_alice".into()),
            vam_cli::cmd::evidence::EvidenceCmd::Export {
                id: bob_run.to_string(),
                out: out.path().to_path_buf(),
            },
        )
        .await
        .expect_err("alice must not export bob")
    });

    mock.assert();
    assert_eq!(
        vam_cli::output::classify(&err),
        (vam_cli::output::ExitCode::NotFound, Some(404))
    );
    let msg = format!("{err:#}");
    assert!(!msg.contains("HTTP"), "{msg}");
    assert!(!msg.contains("404"), "{msg}");
    assert!(msg.contains("Wrong: run not found."), "{msg}");
    assert!(msg.contains("Fix: vam run ls."), "{msg}");
    assert!(
        !out.path().join("manifest.json").exists(),
        "failed export must not write a pack"
    );
}

#[test]
fn run_trajectory_fetches_cas_replay() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let run_urn = "urn:vam:run:local.vam.dev/acme/traj-cli";
    let encoded = vam_cli::client::encode_path_segment(run_urn);
    let replay = serde_json::json!({
        "digest": "abc",
        "events": [{"eventType":"vam.v1.tool.invoked","toolName":"bash"}]
    });
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{encoded}/trajectory"))
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(replay.to_string());
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::run::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::run::RunArgs::try_parse_from(["vam", "trajectory", run_urn]).unwrap(),
        )
        .await
        .unwrap();
    });
    mock.assert();
}

#[test]
fn run_compare_two_gets_and_local_diff() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let a = "urn:vam:run:local.vam.dev/acme/traj-a";
    let b = "urn:vam:run:local.vam.dev/acme/traj-b";
    let enc_a = vam_cli::client::encode_path_segment(a);
    let enc_b = vam_cli::client::encode_path_segment(b);
    let replay_a = serde_json::json!({
        "digest": "da",
        "events": [
            {"eventType":"vam.v1.tool.invoked","toolName":"bash"},
            {"eventType":"vam.v1.policy.decided","toolName":"bash","disposition":"ALLOW"}
        ]
    });
    let replay_b = serde_json::json!({
        "digest": "db",
        "events": [
            {"eventType":"vam.v1.tool.invoked","toolName":"bash"},
            {"eventType":"vam.v1.policy.decided","toolName":"bash","disposition":"ALLOW"},
            {"eventType":"vam.v1.policy.decided","toolName":"bash","disposition":"DENY"}
        ]
    });
    let mock_a = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{enc_a}/trajectory"));
        then.status(200)
            .header("content-type", "application/json")
            .body(replay_a.to_string());
    });
    let mock_b = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/runs/{enc_b}/trajectory"));
        then.status(200)
            .header("content-type", "application/json")
            .body(replay_b.to_string());
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::run::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::run::RunArgs::try_parse_from(["vam", "compare", a, b]).unwrap(),
        )
        .await
        .unwrap();
    });
    mock_a.assert();
    mock_b.assert();
}

#[test]
fn resolve_uses_session_token_from_config() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let file = vam_cli::config::ConfigFile {
        host: Some("http://127.0.0.1:8080".into()),
        session_token: Some("vam_st_session".into()),
        api_key: Some("vam_sk_file".into()),
        ..Default::default()
    };
    let resolved = vam_cli::config::resolve_from(&file, None, None, None, None).unwrap();
    assert_eq!(resolved.api_key, "vam_st_session");
}

#[test]
fn keys_create_sends_user_id_and_vam_api_key() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/keys")
            .header("authorization", "Bearer vam_sk_env")
            .header_exists("Idempotency-Key")
            .json_body(serde_json::json!({
                "name": "ci",
                "userId": "local",
                "scopes": ["node:read"]
            }));
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"key":{"name":"ci","userId":"local","scopes":["node:read"]},"secret":"vam_sk_minted"}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::keys::run(
            Some(server.base_url()),
            Some("vam_sk_env".into()),
            vam_cli::cmd::keys::KeysCmd::Create {
                name: "ci".into(),
                scope: vec!["node:read".into()],
                user_id: Some("local".into()),
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    });
    mock.assert();
}

#[test]
fn resolve_prefers_vam_api_key_over_session() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let file = vam_cli::config::ConfigFile {
        session_token: Some("vam_st_session".into()),
        api_key: Some("vam_sk_file".into()),
        ..Default::default()
    };
    let resolved =
        vam_cli::config::resolve_from(&file, None, None, None, Some("vam_sk_env".into())).unwrap();
    assert_eq!(resolved.api_key, "vam_sk_env");
}

#[test]
fn resolve_prefers_token_flag_over_env_and_session() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let file = vam_cli::config::ConfigFile {
        session_token: Some("vam_st_session".into()),
        api_key: Some("vam_sk_file".into()),
        ..Default::default()
    };
    let resolved = vam_cli::config::resolve_from(
        &file,
        None,
        Some("vam_sk_flag".into()),
        None,
        Some("vam_sk_env".into()),
    )
    .unwrap();
    assert_eq!(resolved.api_key, "vam_sk_flag");
}

#[test]
fn login_stores_token_and_subsequent_resolve_uses_it() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/login")
            .json_body(serde_json::json!({
                "username": "local",
                "password": "secret"
            }));
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{"userId":"local","username":"local","token":"vam_st_login","expiresAt":9999999999,"refreshToken":"vam_rt_login","refreshExpiresAt":9999999999}"#,
            );
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut file = vam_cli::config::ConfigFile::default();
    rt.block_on(async {
        vam_cli::cmd::login::login_into(&server.base_url(), "local", "secret", &mut file)
            .await
            .unwrap();
    });
    mock.assert();
    assert_eq!(file.session_token.as_deref(), Some("vam_st_login"));
    assert_eq!(file.refresh_token.as_deref(), Some("vam_rt_login"));
    assert_eq!(file.session_expires_at, Some(9999999999));

    let resolved = vam_cli::config::resolve_from(&file, None, None, None, None).unwrap();
    assert_eq!(resolved.api_key, "vam_st_login");
}

#[test]
fn logout_clears_session_and_calls_revoke() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/logout")
            .header("authorization", "Bearer vam_st_login");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"revoked":true}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut file = vam_cli::config::ConfigFile {
        host: Some(server.base_url()),
        session_token: Some("vam_st_login".into()),
        refresh_token: Some("vam_rt_login".into()),
        session_expires_at: Some(9999999999),
        api_key: Some("vam_sk_keep".into()),
        ..Default::default()
    };
    rt.block_on(async {
        vam_cli::cmd::login::logout_from(&server.base_url(), &mut file)
            .await
            .unwrap();
    });
    mock.assert();
    assert!(file.session_token.is_none());
    assert!(file.refresh_token.is_none());
    assert!(file.session_expires_at.is_none());
    assert_eq!(file.api_key.as_deref(), Some("vam_sk_keep"));
}

#[test]
fn maybe_refresh_session_updates_expired_credentials() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/login/refresh")
            .json_body(serde_json::json!({ "refreshToken": "vam_rt_old" }));
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{"userId":"local","username":"local","token":"vam_st_refreshed","expiresAt":9999999999,"refreshToken":"vam_rt_new","refreshExpiresAt":9999999999}"#,
            );
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut file = vam_cli::config::ConfigFile {
        host: Some(server.base_url()),
        session_token: Some("vam_st_old".into()),
        refresh_token: Some("vam_rt_old".into()),
        session_expires_at: Some(1),
        ..Default::default()
    };
    rt.block_on(async {
        vam_cli::config::maybe_refresh_session(&server.base_url(), &mut file)
            .await
            .unwrap();
    });
    mock.assert();
    assert_eq!(file.session_token.as_deref(), Some("vam_st_refreshed"));
    assert_eq!(file.refresh_token.as_deref(), Some("vam_rt_new"));
    assert_eq!(file.session_expires_at, Some(9999999999));

    let resolved = vam_cli::config::resolve_from(&file, None, None, None, None).unwrap();
    assert_eq!(resolved.api_key, "vam_st_refreshed");
}

#[test]
fn maybe_refresh_session_401_exits_with_login_hint() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/login/refresh")
            .json_body(serde_json::json!({ "refreshToken": "vam_rt_old" }));
        then.status(401)
            .header("content-type", "application/json")
            .body(r#"{"code":"Unauthenticated","message":"refresh token expired"}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut file = vam_cli::config::ConfigFile {
        host: Some(server.base_url()),
        session_token: Some("vam_st_old".into()),
        refresh_token: Some("vam_rt_old".into()),
        session_expires_at: Some(1),
        ..Default::default()
    };
    let err = rt
        .block_on(async {
            vam_cli::config::maybe_refresh_session(&server.base_url(), &mut file).await
        })
        .unwrap_err();
    mock.assert();
    let (code, _) = vam_cli::output::classify(&err);
    assert_eq!(code, vam_cli::output::ExitCode::Auth);
    let text = format!("{err:#}");
    assert!(text.contains("session expired, run `vam login`"), "{text}");
}

#[test]
fn health_uses_token_after_silent_refresh() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let refresh = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/login/refresh")
            .json_body(serde_json::json!({ "refreshToken": "vam_rt_old" }));
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{"userId":"local","username":"local","token":"vam_st_refreshed","expiresAt":9999999999,"refreshToken":"vam_rt_new","refreshExpiresAt":9999999999}"#,
            );
    });
    let health = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/health")
            .header("authorization", "Bearer vam_st_refreshed");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"status":"ok"}"#);
    });

    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("vam/config.toml");
    let file = vam_cli::config::ConfigFile {
        host: Some(server.base_url()),
        session_token: Some("vam_st_old".into()),
        refresh_token: Some("vam_rt_old".into()),
        session_expires_at: Some(1),
        ..Default::default()
    };
    file.save_to(&config_path).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        vam_cli::cmd::health::run(Some(server.base_url()), None)
            .await
            .unwrap();
        std::env::remove_var("XDG_CONFIG_HOME");
    });
    refresh.assert();
    health.assert();
}

const JIRA_URN: &str = "urn:vam:agent:examples.local/vam/jira-assistant";

fn installed_agents_body(urns: &[&str]) -> String {
    let specs = urns
        .iter()
        .map(|urn| vam_spec::v1::AgentSpec {
            resource_id: Some(vam_spec::v1::ResourceId {
                urn: (*urn).to_string(),
            }),
            display_name: "Agent".into(),
            ..Default::default()
        })
        .collect();
    let resp = vam_spec::v1::ListAgentsResponse {
        specs,
        ..Default::default()
    };
    vam_spec::encoding::proto_to_json("vam.v1.ListAgentsResponse", &resp).unwrap()
}

fn run_vam(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_vam"))
        .current_dir(dir)
        .env_remove("VAM_API_KEY")
        .env_remove("VAM_HOST")
        .env_remove("VAM_TRACEPARENT")
        .args(args)
        .output()
        .expect("spawn vam")
}

fn assert_no_run_json(dir: &std::path::Path) {
    assert!(
        !dir.join("run.json").exists(),
        "a catalog run must not require run.json"
    );
}

#[test]
fn run_short_name_starts_without_run_json() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let list = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/agents");
        then.status(200)
            .header("content-type", "application/json")
            .body(installed_agents_body(&[JIRA_URN]));
    });
    let created = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/runs")
            .body_contains(JIRA_URN)
            .body_contains("summarize the sprint")
            .body_contains("volume://jira-assistant");
        then.status(202)
            .header("content-type", "application/json")
            .body(r#"{"status":{"state":"RUN_STATE_PENDING"}}"#);
    });
    let put = server.mock(|when, then| {
        when.method(PUT).path("/v1/agents");
        then.status(500);
    });

    let dir = tempfile::tempdir().unwrap();
    let output = run_vam(
        dir.path(),
        &[
            "--host",
            &server.base_url(),
            "--token",
            "vam_sk_test",
            "run",
            "jira-assistant",
            "-p",
            "summarize the sprint",
        ],
    );
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    list.assert();
    created.assert();
    assert_eq!(put.hits(), 0);
    assert_no_run_json(dir.path());
}

#[test]
fn run_urn_starts_without_listing_or_run_json() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let list = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/agents");
        then.status(500).body("should not list");
    });
    let created = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/runs")
            .body_contains(JIRA_URN)
            .body_contains("summarize the sprint");
        then.status(202)
            .header("content-type", "application/json")
            .body(r#"{"status":{"state":"RUN_STATE_PENDING"}}"#);
    });

    let dir = tempfile::tempdir().unwrap();
    let output = run_vam(
        dir.path(),
        &[
            "--json",
            "--host",
            &server.base_url(),
            "--token",
            "vam_sk_test",
            "run",
            JIRA_URN,
            "--prompt",
            "summarize the sprint",
        ],
    );
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(parsed.get("error").is_none(), "{parsed}");
    assert_eq!(list.hits(), 0);
    created.assert();
    assert_no_run_json(dir.path());
}

#[test]
fn run_file_flag_posts_and_positional_path_is_usage() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let list = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/agents");
        then.status(500).body("should not list");
    });
    let created = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/runs")
            .body_contains("volume://from-file");
        then.status(202)
            .header("content-type", "application/json")
            .body(r#"{"status":{"state":"RUN_STATE_PENDING"}}"#);
    });

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("run.json"),
        r#"{"spec":{"workspaceRefUri":"volume://from-file","inputs":{"prompt":"from file"}}}"#,
    )
    .unwrap();

    let by_flag = run_vam(
        dir.path(),
        &[
            "--json",
            "--host",
            &server.base_url(),
            "--token",
            "vam_sk_test",
            "run",
            "--file",
            "run.json",
        ],
    );
    assert!(
        by_flag.status.success(),
        "flag stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&by_flag.stdout),
        String::from_utf8_lossy(&by_flag.stderr)
    );
    let stdout = String::from_utf8(by_flag.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(parsed.get("error").is_none(), "{parsed}");

    let by_start = run_vam(
        dir.path(),
        &[
            "--json",
            "--host",
            &server.base_url(),
            "--token",
            "vam_sk_test",
            "run",
            "start",
            "run.json",
        ],
    );
    assert_eq!(by_start.status.code(), Some(2), "{by_start:?}");
    let start_out = String::from_utf8(by_start.stdout).unwrap();
    let start_err: serde_json::Value = serde_json::from_str(start_out.trim()).unwrap();
    assert_eq!(start_err["error"]["exitCode"], 2);
    assert!(
        start_err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("vam run --file"),
        "{start_err}"
    );
    assert!(by_start.stderr.is_empty());

    let positional = run_vam(dir.path(), &["--json", "run", "run.json"]);
    assert_eq!(positional.status.code(), Some(2));
    let pos_out = String::from_utf8(positional.stdout).unwrap();
    assert!(pos_out.contains("vam run --file"), "{pos_out}");
    assert!(positional.stderr.is_empty());

    assert_eq!(created.hits(), 1);
    assert_eq!(list.hits(), 0);
}

#[test]
fn run_workspace_thread_prompt_posts_without_agent_or_continue_verb() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let list = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/agents");
        then.status(500).body("should not list");
    });
    let created = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/runs")
            .body_contains("sp_k3q7xw2m5b4n6a7c3d2e3f4g5h")
            .body_contains("feature-login")
            .body_contains("add a badge")
            .matches(|req| {
                let body = req.body.as_deref().unwrap_or_default();
                let text = String::from_utf8_lossy(body);
                !text.contains("agentRef") && !text.contains("agent_ref")
            });
        then.status(202)
            .header("content-type", "application/json")
            .body(r#"{"status":{"state":"RUN_STATE_PENDING"},"spaceId":"sp_k3q7xw2m5b4n6a7c3d2e3f4g5h","threadId":"th_k3q7xw2m5b4n6a7c3d2e3f4g5h","threadRef":"feature-login"}"#);
    });

    let dir = tempfile::tempdir().unwrap();
    let output = run_vam(
        dir.path(),
        &[
            "--json",
            "--host",
            &server.base_url(),
            "--token",
            "vam_sk_test",
            "run",
            "--workspace",
            "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h",
            "--thread",
            "feature-login",
            "-p",
            "add a badge",
        ],
    );
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(parsed.get("error").is_none(), "{parsed}");
    created.assert();
    assert_eq!(list.hits(), 0);
    assert_no_run_json(dir.path());

    let gone = run_vam(
        dir.path(),
        &[
            "--json",
            "run",
            "continue",
            "--workspace",
            "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h",
            "--thread",
            "feature-login",
            "-p",
            "add a badge",
        ],
    );
    assert_eq!(gone.status.code(), Some(2), "{gone:?}");
    let gone_out = String::from_utf8(gone.stdout).unwrap();
    let gone_err: serde_json::Value = serde_json::from_str(gone_out.trim()).unwrap();
    assert_eq!(gone_err["error"]["code"], "usage");
    assert_eq!(gone_err["error"]["exitCode"], 2);
    let message = gone_err["error"]["message"].as_str().unwrap();
    assert!(message.contains("not a command"), "{message}");
    assert!(message.contains("--workspace"), "{message}");
    assert!(message.contains("-p"), "{message}");
    assert!(gone.stderr.is_empty());

    let no_prompt = run_vam(
        dir.path(),
        &[
            "--json",
            "run",
            "--workspace",
            "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h",
            "--thread",
            "feature-login",
        ],
    );
    assert_eq!(no_prompt.status.code(), Some(2));
    let no_prompt_out = String::from_utf8(no_prompt.stdout).unwrap();
    assert!(no_prompt_out.contains("-p"), "{no_prompt_out}");
    assert!(no_prompt.stderr.is_empty());
    assert_eq!(created.hits(), 1);
}

#[test]
fn run_by_ids_prints_the_place_line_and_a_label_is_never_a_lookup() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let sp = "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h";
    let th = "th_k3q7xw2m5b4n6a7c3d2e3f4g5h";
    let server = MockServer::start();
    let created = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/runs")
            .body_contains(format!("\"spaceId\":\"{sp}\""))
            .body_contains(format!("\"threadId\":\"{th}\""))
            .matches(|req| {
                !String::from_utf8_lossy(req.body.as_deref().unwrap_or_default())
                    .contains("threadRef")
            });
        then.status(202)
            .header("content-type", "application/json")
            .body(format!(
                r#"{{"status":{{"state":"RUN_STATE_PENDING"}},"spaceId":"{sp}","threadId":"{th}","threadRef":"my plan"}}"#
            ));
    });
    let space = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/spaces/{sp}"));
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(r#"{{"spaceId":"{sp}","name":"my notes"}}"#));
    });
    let dir = tempfile::tempdir().unwrap();
    let output = run_vam(
        dir.path(),
        &[
            "--host",
            &server.base_url(),
            "--token",
            "vam_sk_test",
            "run",
            "--workspace",
            sp,
            "--thread",
            th,
            "-p",
            "next",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "space {sp} (my notes) \u{b7} thread {th} (my plan)"
        )),
        "{stderr}"
    );
    created.assert();
    space.assert();

    // A label for the Space is refused before any request.
    let refused = run_vam(
        dir.path(),
        &["--json", "run", "--workspace", "my notes", "-p", "next"],
    );
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    assert_eq!(created.hits(), 1);
}

#[test]
fn run_without_agent_exits_usage_without_prompting() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let dir = tempfile::tempdir().unwrap();
    let output = run_vam(dir.path(), &["--json", "run"]);
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["error"]["exitCode"], 2);
    assert_eq!(parsed["error"]["code"], "usage");
    let message = parsed["error"]["message"].as_str().unwrap();
    assert!(message.contains("vam run"), "{message}");
    assert!(message.contains("-p"), "{message}");
    assert!(output.stderr.is_empty());
}

#[test]
fn run_unknown_short_name_exits_not_found() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let list = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/agents");
        then.status(200)
            .header("content-type", "application/json")
            .body(installed_agents_body(&[
                "urn:vam:agent:examples.local/vam/bun-assistant",
            ]));
    });
    let created = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/v1/runs");
        then.status(500);
    });

    let dir = tempfile::tempdir().unwrap();
    let output = run_vam(
        dir.path(),
        &[
            "--host",
            &server.base_url(),
            "--token",
            "vam_sk_test",
            "run",
            "jira-assistant",
            "-p",
            "hello",
        ],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("vam agent ls"), "{stderr}");
    list.assert();
    assert_eq!(created.hits(), 0);
    assert_no_run_json(dir.path());
}

#[test]
fn workspace_ls_hits_list_route() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let listing = serde_json::json!({
        "workspaceId": "assistant",
        "head": "abc123",
        "files": [
            {"path": "workspace/PLAN.md", "sizeBytes": 120, "mode": 420},
            {"path": "workspace/notes/a.md", "sizeBytes": 42, "mode": 420}
        ],
        "truncated": false
    });
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/workspaces/sp_k3q7xw2m5b4n6a7c3d2e3f4g5h/files")
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(listing.to_string());
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::workspace::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::workspace::WorkspaceCmd::Ls {
                workspace: "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h".into(),
            },
        )
        .await
        .unwrap();
    });
    mock.assert();
}

#[test]
fn workspace_ls_json_fetches_api_body() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let listing = serde_json::json!({
        "workspaceId": "assistant",
        "head": "abc123",
        "files": [
            {"path": "workspace/PLAN.md", "sizeBytes": 120, "mode": 420}
        ],
        "truncated": false
    });
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/workspaces/sp_k3q7xw2m5b4n6a7c3d2e3f4g5h/files")
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(listing.to_string());
    });

    vam_cli::output::set_json(true);
    let result = {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            vam_cli::cmd::workspace::run(
                Some(server.base_url()),
                Some("vam_sk_test".into()),
                vam_cli::cmd::workspace::WorkspaceCmd::Ls {
                    workspace: "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h".into(),
                },
            )
            .await
        })
    };
    vam_cli::output::set_json(false);
    result.unwrap();
    mock.assert();
}

#[test]
fn workspace_cat_writes_file_and_shorthand_hits_encoded_url() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let body_bytes = b"hello workspace file\n";
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/workspaces/sp_k3q7xw2m5b4n6a7c3d2e3f4g5h/files/workspace/notes/a.md")
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/octet-stream")
            .body(body_bytes.as_slice());
    });

    let out = tempfile::tempdir().unwrap();
    let out_file = out.path().join("a.md");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::workspace::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::workspace::WorkspaceCmd::Cat {
                path: "notes/a.md".into(),
                workspace: "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h".into(),
                out: Some(out_file.clone()),
            },
        )
        .await
        .unwrap();
    });

    mock.assert();
    assert_eq!(std::fs::read(&out_file).unwrap(), body_bytes);
}

#[test]
fn workspace_cat_json_requires_out_and_prints_metadata() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let body_bytes = b"meta";
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/workspaces/sp_k3q7xw2m5b4n6a7c3d2e3f4g5h/files/workspace/PLAN.md")
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/octet-stream")
            .body(body_bytes.as_slice());
    });

    let out = tempfile::tempdir().unwrap();
    let out_file = out.path().join("PLAN.md");
    vam_cli::output::set_json(true);
    let err = {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            vam_cli::cmd::workspace::run(
                Some(server.base_url()),
                Some("vam_sk_test".into()),
                vam_cli::cmd::workspace::WorkspaceCmd::Cat {
                    path: "workspace/PLAN.md".into(),
                    workspace: "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h".into(),
                    out: None,
                },
            )
            .await
            .expect_err("--json without -o")
        })
    };
    assert_eq!(
        vam_cli::output::classify(&err).0,
        vam_cli::output::ExitCode::Usage
    );
    assert_eq!(mock.hits(), 0);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::workspace::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::workspace::WorkspaceCmd::Cat {
                path: "workspace/PLAN.md".into(),
                workspace: "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h".into(),
                out: Some(out_file.clone()),
            },
        )
        .await
        .unwrap();
    });
    vam_cli::output::set_json(false);
    mock.assert();
    assert_eq!(std::fs::read(&out_file).unwrap(), body_bytes);
}

#[test]
fn workspace_cat_404_is_not_found() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/workspaces/sp_k3q7xw2m5b4n6a7c3d2e3f4g5h/files/workspace/missing.md")
            .header("authorization", "Bearer vam_sk_test");
        then.status(404)
            .header("content-type", "application/json")
            .body(r#"{"code":"NotFound","message":"file not found"}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let err = rt.block_on(async {
        vam_cli::cmd::workspace::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::workspace::WorkspaceCmd::Cat {
                path: "missing.md".into(),
                workspace: "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h".into(),
                out: None,
            },
        )
        .await
        .expect_err("404")
    });
    mock.assert();
    assert_eq!(
        vam_cli::output::classify(&err),
        (vam_cli::output::ExitCode::NotFound, Some(404))
    );
}

#[test]
fn workspace_cat_413_is_invalid() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/workspaces/sp_k3q7xw2m5b4n6a7c3d2e3f4g5h/files/workspace/big.bin")
            .header("authorization", "Bearer vam_sk_test");
        then.status(413)
            .header("content-type", "application/json")
            .body(r#"{"code":"InvalidArgument","message":"file size exceeds maximum","retryable":false}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let err = rt.block_on(async {
        vam_cli::cmd::workspace::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::workspace::WorkspaceCmd::Cat {
                path: "big.bin".into(),
                workspace: "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h".into(),
                out: None,
            },
        )
        .await
        .expect_err("413")
    });
    mock.assert();
    assert_eq!(
        vam_cli::output::classify(&err),
        (vam_cli::output::ExitCode::Invalid, Some(413))
    );
}

#[test]
fn workspace_put_uploads_bytes_and_prints_head() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let local = tempfile::tempdir().unwrap();
    let local_file = local.path().join("a.md");
    std::fs::write(&local_file, b"hello from upload\n").unwrap();
    let response = serde_json::json!({
        "workspaceId": "assistant",
        "path": "workspace/notes/a.md",
        "head": "abc123",
        "sizeBytes": 18
    });
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::PUT)
            .path("/v1/workspaces/sp_k3q7xw2m5b4n6a7c3d2e3f4g5h/files/workspace/notes/a.md")
            .header("authorization", "Bearer vam_sk_test")
            .header("content-type", "application/octet-stream")
            .body("hello from upload\n");
        then.status(200)
            .header("content-type", "application/json")
            .body(response.to_string());
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        vam_cli::cmd::workspace::run(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            vam_cli::cmd::workspace::WorkspaceCmd::Put {
                path: "notes/a.md".into(),
                file: local_file,
                workspace: "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h".into(),
            },
        )
        .await
        .unwrap();
    });
    mock.assert();
}

#[test]
fn workspace_put_json_prints_api_body() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let local = tempfile::tempdir().unwrap();
    let local_file = local.path().join("a.md");
    std::fs::write(&local_file, b"hello").unwrap();
    let response = serde_json::json!({
        "workspaceId": "assistant",
        "path": "workspace/notes/a.md",
        "head": "abc123",
        "sizeBytes": 5
    });
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::PUT)
            .path("/v1/workspaces/sp_k3q7xw2m5b4n6a7c3d2e3f4g5h/files/workspace/notes/a.md")
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(response.to_string());
    });

    vam_cli::output::set_json(true);
    let result = {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            vam_cli::cmd::workspace::run(
                Some(server.base_url()),
                Some("vam_sk_test".into()),
                vam_cli::cmd::workspace::WorkspaceCmd::Put {
                    path: "notes/a.md".into(),
                    file: local_file,
                    workspace: "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h".into(),
                },
            )
            .await
        })
    };
    vam_cli::output::set_json(false);
    result.unwrap();
    mock.assert();
}

#[test]
fn workspace_put_busy_workspace_is_conflict() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let local = tempfile::tempdir().unwrap();
    let local_file = local.path().join("a.md");
    std::fs::write(&local_file, b"hello").unwrap();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::PUT)
            .path("/v1/workspaces/sp_k3q7xw2m5b4n6a7c3d2e3f4g5h/files/workspace/notes/a.md")
            .header("authorization", "Bearer vam_sk_test");
        then.status(409)
            .header("content-type", "application/json")
            .body(r#"{"code":"FailedPrecondition","message":"agent is running on workspace 'operator--assistant' (session sess-1, run run-1)","retryable":true}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let err = rt
        .block_on(async {
            vam_cli::cmd::workspace::run(
                Some(server.base_url()),
                Some("vam_sk_test".into()),
                vam_cli::cmd::workspace::WorkspaceCmd::Put {
                    path: "notes/a.md".into(),
                    file: local_file,
                    workspace: "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h".into(),
                },
            )
            .await
        })
        .expect_err("409");
    mock.assert();
    let text = format!("{err:#}");
    assert!(text.contains("idle workspace"), "{text}");
    assert!(text.contains("vam session terminate sess-1"), "{text}");
    assert_eq!(
        vam_cli::output::classify(&err),
        (vam_cli::output::ExitCode::Conflict, Some(409))
    );
}

#[test]
fn workspace_put_413_mentions_upload_limit() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let local = tempfile::tempdir().unwrap();
    let local_file = local.path().join("a.md");
    std::fs::write(&local_file, b"hello").unwrap();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::PUT)
            .path("/v1/workspaces/sp_k3q7xw2m5b4n6a7c3d2e3f4g5h/files/workspace/big.bin")
            .header("authorization", "Bearer vam_sk_test");
        then.status(413)
            .header("content-type", "application/json")
            .body(r#"{"code":"InvalidArgument","message":"file size exceeds maximum","retryable":false}"#);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let err = rt
        .block_on(async {
            vam_cli::cmd::workspace::run(
                Some(server.base_url()),
                Some("vam_sk_test".into()),
                vam_cli::cmd::workspace::WorkspaceCmd::Put {
                    path: "big.bin".into(),
                    file: local_file,
                    workspace: "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h".into(),
                },
            )
            .await
        })
        .expect_err("413");
    mock.assert();
    let text = format!("{err:#}");
    assert!(text.contains("32 MiB upload limit"), "{text}");
    assert!(text.contains("vam workspace put"), "{text}");
}

#[test]
fn workspace_put_rejects_local_file_over_limit_without_http() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let local = tempfile::tempdir().unwrap();
    let local_file = local.path().join("big.bin");
    std::fs::write(&local_file, vec![0u8; 32 * 1024 * 1024 + 1]).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let err = rt
        .block_on(async {
            vam_cli::cmd::workspace::run(
                Some(server.base_url()),
                Some("vam_sk_test".into()),
                vam_cli::cmd::workspace::WorkspaceCmd::Put {
                    path: "big.bin".into(),
                    file: local_file,
                    workspace: "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h".into(),
                },
            )
            .await
        })
        .expect_err("local size");
    let text = err.to_string();
    assert!(text.contains("32 MiB"), "{text}");
    assert_eq!(
        vam_cli::output::classify(&err),
        (vam_cli::output::ExitCode::Usage, None)
    );
}

#[test]
fn client_get_bytes_returns_octet_stream() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/workspaces/sp_k3q7xw2m5b4n6a7c3d2e3f4g5h/files/workspace/x.bin")
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/octet-stream")
            .body(&[0u8, 1, 2, 255][..]);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let bytes = rt.block_on(async {
        let cfg = ResolvedConfig {
            host: server.base_url(),
            api_key: "vam_sk_test".into(),
        };
        let client = Client::new(&cfg).unwrap();
        client
            .get_bytes("/v1/workspaces/sp_k3q7xw2m5b4n6a7c3d2e3f4g5h/files/workspace/x.bin")
            .await
            .unwrap()
    });
    mock.assert();
    assert_eq!(bytes, vec![0, 1, 2, 255]);
}

#[test]
fn agent_release_at_a_version_prints_the_released_spec_as_json() {
    let urn = "urn:vam:agent:local.vam.dev/acme/release-json";
    let server = MockServer::start();
    let release = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path(format!(
            "/v1/agents/{}/release",
            vam_cli::client::encode_path_segment(urn)
        ));
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(
                r#"{{"resourceId":{{"urn":"{urn}"}},"displayName":"Release JSON"}}"#
            ));
    });
    let dir = tempfile::tempdir().expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_vam"))
        .current_dir(dir.path())
        .env_remove("VAM_API_KEY")
        .env_remove("VAM_HOST")
        .args([
            "--json",
            "--host",
            &server.base_url(),
            "--token",
            "vam_sk_test",
            "agent",
            "release",
            urn,
            "--version",
            "1.0.0",
        ])
        .output()
        .expect("spawn vam");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    release.assert();
    let stdout: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("one JSON document on stdout");
    assert_eq!(stdout["resourceId"]["urn"].as_str(), Some(urn));
}

#[test]
fn agent_release_sends_the_note_in_the_request_body() {
    let urn = "urn:vam:agent:local.vam.dev/acme/release-note";
    let server = MockServer::start();
    let release = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path(format!(
                "/v1/agents/{}/release",
                vam_cli::client::encode_path_segment(urn)
            ))
            .body_contains("adds jira");
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(r#"{{"resourceId":{{"urn":"{urn}"}}}}"#));
    });
    let dir = tempfile::tempdir().expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_vam"))
        .current_dir(dir.path())
        .env_remove("VAM_API_KEY")
        .env_remove("VAM_HOST")
        .args([
            "--host",
            &server.base_url(),
            "--token",
            "vam_sk_test",
            "agent",
            "release",
            urn,
            "--version",
            "1.1.0",
            "--note",
            "adds jira",
        ])
        .output()
        .expect("spawn vam");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    release.assert();
}

#[test]
fn agent_versions_prints_a_table_and_json_flag_prints_the_response() {
    let urn = "urn:vam:agent:local.vam.dev/acme/versions-cli";
    let server = MockServer::start();
    let versions_body = r#"{"versions":[
        {"version":{"prerelease":"draft"},"channel":"DRAFT","digest":"d","updatedAt":"2026-10-10T16:00:00Z"},
        {"version":{"major":1,"minor":1},"channel":"RELEASED","digest":"e","releasedBy":"alice","releasedAt":"2026-10-10T16:10:00Z","note":"adds jira"}
    ]}"#;
    let versions = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path(format!(
            "/v1/agents/{}/versions",
            vam_cli::client::encode_path_segment(urn)
        ));
        then.status(200)
            .header("content-type", "application/json")
            .body(versions_body);
    });
    let base = server.base_url();
    let run = |extra: &[&str]| {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut args = vec![
            "--host",
            base.as_str(),
            "--token",
            "vam_sk_test",
            "agent",
            "versions",
            urn,
        ];
        args.extend_from_slice(extra);
        Command::new(env!("CARGO_BIN_EXE_vam"))
            .current_dir(dir.path())
            .env_remove("VAM_API_KEY")
            .env_remove("VAM_HOST")
            .args(args)
            .output()
            .expect("spawn vam")
    };

    let table = run(&[]);
    assert!(
        table.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&table.stderr)
    );
    let stdout = String::from_utf8_lossy(&table.stdout);
    assert!(stdout.contains("draft"), "{stdout}");
    assert!(stdout.contains("1.1.0"), "{stdout}");
    assert!(stdout.contains("alice"), "{stdout}");
    assert!(stdout.contains("adds jira"), "{stdout}");

    let json = run(&["--json"]);
    assert!(json.status.success());
    let parsed: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("raw JSON on stdout");
    assert_eq!(parsed["versions"].as_array().map(Vec::len), Some(2));
    versions.assert_hits(2);
}

#[test]
fn agent_release_conflict_exits_non_zero_and_names_the_next_free_version() {
    let urn = "urn:vam:agent:local.vam.dev/acme/release-conflict";
    let server = MockServer::start();
    let release = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path(format!(
            "/v1/agents/{}/release",
            vam_cli::client::encode_path_segment(urn)
        ));
        then.status(409)
            .header("content-type", "application/json")
            .body(format!(
                r#"{{"code":"Conflict","message":"agent '{urn}' 1.0.0 is already released with different content; the next free version is 1.0.1","retryable":false}}"#
            ));
    });
    let dir = tempfile::tempdir().expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_vam"))
        .current_dir(dir.path())
        .env_remove("VAM_API_KEY")
        .env_remove("VAM_HOST")
        .args([
            "--host",
            &server.base_url(),
            "--token",
            "vam_sk_test",
            "agent",
            "release",
            urn,
            "--version",
            "1.0.0",
        ])
        .output()
        .expect("spawn vam");
    assert!(!out.status.success(), "conflict must exit non-zero");
    release.assert();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("the next free version is 1.0.1"),
        "stderr: {stderr}"
    );
}

#[test]
fn agent_release_with_unreleased_dependencies_prints_each_offender_and_the_fix() {
    let urn = "urn:vam:agent:local.vam.dev/acme/release-deps";
    let server = MockServer::start();
    let release = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path(format!(
            "/v1/agents/{}/release",
            vam_cli::client::encode_path_segment(urn)
        ));
        then.status(409)
            .header("content-type", "application/json")
            .body(format!(
                r#"{{"code":"FailedPrecondition","message":"cannot release agent '{urn}': 2 dependencies are not released\n  skill 'urn:vam:skill:local.vam.dev/acme/jira' has no released version: release it first, then release this agent.\n  skill 'urn:vam:skill:local.vam.dev/acme/slack' has no released version: release it first, then release this agent.","retryable":false}}"#
            ));
    });
    let out = run_vam_against(&server, &["agent", "release", urn, "--version", "1.0.0"]);
    assert!(!out.status.success(), "refusal must exit non-zero");
    release.assert();
    let stderr = String::from_utf8_lossy(&out.stderr);
    for name in ["jira", "slack"] {
        assert!(
            stderr.contains(&format!(
                "\n  skill 'urn:vam:skill:local.vam.dev/acme/{name}' has no released version: release it first, then release this agent."
            )),
            "stderr: {stderr}"
        );
        assert!(
            stderr.contains(&format!(
                "vam skill release urn:vam:skill:local.vam.dev/acme/{name} --version <X.Y.Z>"
            )),
            "stderr: {stderr}"
        );
    }
}

fn suggestion_mock<'a>(server: &'a MockServer, urn: &str) -> httpmock::Mock<'a> {
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!(
                "/v1/agents/{}/release-suggestion",
                vam_cli::client::encode_path_segment(urn)
            ))
            .header("authorization", "Bearer vam_sk_test");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{"latest":{"major":1},"suggested":{"major":1,"minor":1},"bump":"MINOR","reasons":["added skill jira"]}"#,
            );
    })
}

fn run_vam_against(server: &MockServer, args: &[&str]) -> std::process::Output {
    let dir = tempfile::tempdir().expect("tempdir");
    Command::new(env!("CARGO_BIN_EXE_vam"))
        .current_dir(dir.path())
        .env_remove("VAM_API_KEY")
        .env_remove("VAM_HOST")
        .stdin(std::process::Stdio::null())
        .args(["--host", &server.base_url(), "--token", "vam_sk_test"])
        .args(args)
        .output()
        .expect("spawn vam")
}

#[test]
fn agent_release_without_version_and_without_a_tty_fails_naming_the_suggestion() {
    let urn = "urn:vam:agent:local.vam.dev/acme/suggest-release";
    let server = MockServer::start();
    let suggestion = suggestion_mock(&server, urn);
    let release = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path_matches(httpmock::Regex::new("/release$").unwrap());
        then.status(200).body("{}");
    });
    let out = run_vam_against(&server, &["agent", "release", urn]);
    assert!(!out.status.success(), "must not release without --version");
    suggestion.assert();
    release.assert_hits(0);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--version is required; suggested: 1.1.0 (minor: added skill jira)"),
        "stderr: {stderr}"
    );
}

#[test]
fn agent_suggest_version_json_prints_the_suggestion() {
    let urn = "urn:vam:agent:local.vam.dev/acme/suggest-json";
    let server = MockServer::start();
    let suggestion = suggestion_mock(&server, urn);
    let out = run_vam_against(&server, &["agent", "suggest-version", urn, "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    suggestion.assert();
    let stdout: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("one JSON document on stdout");
    assert_eq!(stdout["bump"], "MINOR");
    assert_eq!(stdout["suggested"]["minor"], 1);
    assert_eq!(stdout["reasons"][0], "added skill jira");
}

#[test]
fn agent_suggest_version_prints_a_readable_line() {
    let urn = "urn:vam:agent:local.vam.dev/acme/suggest-human";
    let server = MockServer::start();
    let _suggestion = suggestion_mock(&server, urn);
    let out = run_vam_against(&server, &["agent", "suggest-version", urn]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("suggested: 1.1.0 (minor: added skill jira)"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("latest:    1.0.0"), "stdout: {stdout}");
}

#[test]
fn version_flag_prints_crate_version_and_git_sha() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = run_vam(dir.path(), &["--version"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("vam "), "stdout: {stdout}");
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "stdout: {stdout}"
    );
    assert!(stdout.contains(" ("), "stdout: {stdout}");
}

#[test]
fn users_create_posts_role_and_env_password_with_idempotency_key() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/users")
            .header("authorization", "Bearer vam_sk_test")
            .header_exists("Idempotency-Key")
            .json_body_partial(
                r#"{"username":"ops-bot","role":"publisher","password":"pw-from-env-only"}"#,
            );
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{"user":{"userId":"ops-bot","username":"ops-bot","role":"publisher","hasPassword":true}}"#,
            );
    });
    let dir = tempfile::tempdir().expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_vam"))
        .current_dir(dir.path())
        .env_remove("VAM_API_KEY")
        .env_remove("VAM_HOST")
        .env("VAM_TEST_NEW_USER_PASSWORD", "pw-from-env-only")
        .stdin(std::process::Stdio::null())
        .args([
            "--host",
            &server.base_url(),
            "--token",
            "vam_sk_test",
            "users",
            "create",
            "--username",
            "ops-bot",
            "--role",
            "publisher",
            "--password-from-env",
            "VAM_TEST_NEW_USER_PASSWORD",
        ])
        .output()
        .expect("spawn vam");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    mock.assert();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ops-bot"), "stdout: {stdout}");
    assert!(!stdout.contains("pw-from-env-only"), "stdout: {stdout}");
}

#[test]
fn users_create_refuses_a_password_on_argv_before_any_request() {
    let server = MockServer::start();
    let none = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/v1/users");
        then.status(200);
    });
    let out = run_vam_against(
        &server,
        &[
            "users",
            "create",
            "--username",
            "ops-bot",
            "--role",
            "member",
            "--password",
            "argv-secret",
        ],
    );
    assert!(!out.status.success());
    none.assert_hits(0);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!text.contains("argv-secret"), "output: {text}");
}

#[test]
fn users_create_names_the_missing_password_env_var_without_a_request() {
    let server = MockServer::start();
    let none = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/v1/users");
        then.status(200);
    });
    let out = run_vam_against(
        &server,
        &[
            "users",
            "create",
            "--username",
            "ops-bot",
            "--role",
            "member",
            "--password-from-env",
            "VAM_TEST_UNSET_PASSWORD_VAR",
        ],
    );
    assert!(!out.status.success());
    none.assert_hits(0);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("VAM_TEST_UNSET_PASSWORD_VAR"),
        "stderr: {stderr}"
    );
}

#[test]
fn users_create_rejects_roles_outside_the_offered_set() {
    let server = MockServer::start();
    let none = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/v1/users");
        then.status(200);
    });
    let out = run_vam_against(
        &server,
        &[
            "users",
            "create",
            "--username",
            "ops-bot",
            "--role",
            "operator",
            "--password-from-env",
            "VAM_TEST_UNSET_PASSWORD_VAR",
        ],
    );
    assert!(!out.status.success());
    none.assert_hits(0);
}

#[test]
fn space_create_rename_and_thread_rename_call_the_id_routes() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let sp = "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h";
    let th = "th_k3q7xw2m5b4n6a7c3d2e3f4g5h";
    let agent = "urn:vam:agent:local.vam.dev/acme/a";
    let server = MockServer::start();
    let create = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/spaces")
            .header_exists("idempotency-key")
            .body_contains(agent)
            .body_contains("my notes");
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(r#"{{"spaceId":"{sp}","name":"my notes"}}"#));
    });
    let rename = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path(format!("/v1/spaces/{sp}"))
            .body_contains("new name");
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(r#"{{"spaceId":"{sp}","name":"new name"}}"#));
    });
    let thread = server.mock(|when, then| {
        when.method(httpmock::Method::PATCH)
            .path(format!("/v1/spaces/{sp}/threads/{th}"))
            .body_contains("plan b");
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(r#"{{"threadId":"{th}","name":"plan b"}}"#));
    });
    let dir = tempfile::tempdir().unwrap();
    let host = server.base_url();
    let base = ["--host", host.as_str(), "--token", "vam_sk_test"];
    let go = |extra: &[&str]| {
        let mut args: Vec<&str> = base.to_vec();
        args.extend_from_slice(extra);
        run_vam(dir.path(), &args)
    };
    let out = go(&["space", "create", "--agent", agent, "my notes"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("{sp} (my notes)")));
    let out = go(&["space", "rename", sp, "new name"]);
    assert!(out.status.success(), "{out:?}");
    let out = go(&["thread", "rename", "--space", sp, th, "plan b"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("{th} (plan b)")));
    // A label is refused before any request.
    let out = go(&["space", "rename", "my notes", "x"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    create.assert();
    rename.assert();
    thread.assert();
}
