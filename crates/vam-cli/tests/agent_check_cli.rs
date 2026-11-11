use httpmock::MockServer;
use tokio::runtime::Runtime;
use vam_cli::cmd::catalog::{run_agent, AgentCmd};

const URN: &str = "urn:vam:agent:local.vam.dev/acme/composer";

const RESPONSE: &str = r#"{
    "agentRef": {"resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/composer"}, "version": {"prerelease": "draft", "major": 1}},
    "playbooks": [{
        "urn": "urn:vam:playbook:local.vam.dev/acme/triage", "role": "default",
        "version": {"major": 1},
        "requiredSkills": [{"urn": "urn:vam:skill:local.vam.dev/acme/jira", "version": {"major": 1}}]
    }],
    "skills": [{"urn": "urn:vam:skill:local.vam.dev/acme/jira", "version": {"major": 1}, "capabilities": ["tool:jira.search"], "requiredBy": ["urn:vam:playbook:local.vam.dev/acme/triage"]}]
}"#;

fn check(server: &MockServer, version: Option<&str>, draft: bool) -> anyhow::Result<()> {
    Runtime::new().unwrap().block_on(run_agent(
        Some(server.base_url()),
        Some("vam_sk_test".into()),
        AgentCmd::Check {
            urn: URN.into(),
            version: version.map(str::to_string),
            draft,
            json: false,
        },
    ))
}

#[test]
fn check_with_draft_posts_the_draft_ref_and_exits_zero_with_a_missing_skill() {
    let server = MockServer::start();
    let post = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/agent-checks")
            .body_contains("\"prerelease\":\"draft\"")
            .body_contains("composer");
        then.status(200)
            .header("content-type", "application/json")
            .body(RESPONSE);
    });
    let result = check(&server, None, true);
    assert!(result.is_ok(), "{result:?}");
    post.assert();
}

#[test]
fn check_with_version_posts_that_release() {
    let server = MockServer::start();
    let post = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/agent-checks")
            .body_contains("\"minor\":2");
        then.status(200)
            .header("content-type", "application/json")
            .body(RESPONSE);
    });
    let result = check(&server, Some("1.2.3"), false);
    assert!(result.is_ok(), "{result:?}");
    post.assert();
}

#[test]
fn check_exits_nonzero_when_the_server_errors() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/v1/agent-checks");
        then.status(404)
            .header("content-type", "application/json")
            .body(r#"{"error":"not found"}"#);
    });
    assert!(check(&server, None, true).is_err());
}
