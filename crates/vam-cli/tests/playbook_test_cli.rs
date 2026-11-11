use httpmock::MockServer;
use tokio::runtime::Runtime;
use vam_cli::cmd::catalog::{run_playbook, PlaybookCmd};

const PLAYBOOK: &str = r#"{
    "resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/cli-test"},
    "displayName": "CLI test",
    "steps": [{"id": "a", "kind": "STEP_KIND_TASK", "mode": "STEP_MODE_AGENTIC", "instruction": "Do it."}]
}"#;

const SCENARIO: &str = r#"{
    "actions": [],
    "expect": {"steps": [{"id": "a", "state": "completed"}]}
}"#;

fn write_inputs(dir: &std::path::Path) -> (String, String) {
    let playbook = dir.join("playbook.json");
    let scenario = dir.join("scenario.json");
    std::fs::write(&playbook, PLAYBOOK).unwrap();
    std::fs::write(&scenario, SCENARIO).unwrap();
    (
        playbook.to_string_lossy().into_owned(),
        scenario.to_string_lossy().into_owned(),
    )
}

#[test]
fn playbook_test_posts_the_scenario_and_passes_when_every_trial_passes() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    let (path, scenario) = write_inputs(dir.path());

    let post = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/playbook-tests")
            .body_contains("\"repeat\":2")
            .body_contains("cli-test")
            .body_contains("\"expect\"");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"passed":2,"repeat":2,"passK":1.0,"failures":[]}"#);
    });

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(run_playbook(
        Some(server.base_url()),
        Some("vam_sk_test".into()),
        PlaybookCmd::Test {
            path,
            scenario,
            repeat: 2,
        },
    ));
    assert!(result.is_ok(), "{result:?}");
    post.assert();
}

#[test]
fn playbook_test_exits_nonzero_when_a_trial_fails() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    let (path, scenario) = write_inputs(dir.path());

    let post = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/playbook-tests")
            .body_contains("\"repeat\":3");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"passed":1,"repeat":3,"passK":0.037,"failures":["trial 2: boom"]}"#);
    });

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(run_playbook(
        Some(server.base_url()),
        Some("vam_sk_test".into()),
        PlaybookCmd::Test {
            path,
            scenario,
            repeat: 3,
        },
    ));
    assert!(result.is_err(), "a failed trial must exit non-zero");
    post.assert();
}

#[test]
fn playbook_test_rejects_an_unreadable_scenario_before_posting() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write_inputs(dir.path());
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "{not json").unwrap();

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(run_playbook(
        Some(server.base_url()),
        Some("vam_sk_test".into()),
        PlaybookCmd::Test {
            path,
            scenario: bad.to_string_lossy().into_owned(),
            repeat: 1,
        },
    ));
    assert!(result.is_err());
}
