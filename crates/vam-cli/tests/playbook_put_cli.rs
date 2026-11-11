use std::process::{Command, Output};

use httpmock::{Method, MockServer};

const TRACKING_ONLY: &str = r#"{
    "resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/track"},
    "displayName": "Track",
    "steps": [{"id": "task", "kind": "STEP_KIND_TASK", "mode": "STEP_MODE_AGENTIC",
               "instruction": "Do it."}]
}"#;

const SAVED: &str =
    r#"{"spec": {"resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/track"}}}"#;

fn vam_put(host: &str, path: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vam"))
        .args([
            "--host",
            host,
            "--token",
            "vam_sk_test",
            "playbook",
            "put",
            path,
        ])
        .output()
        .expect("run vam")
}

fn write_playbook(dir: &std::path::Path, body: &str) -> String {
    let path = dir.join("playbook.json");
    std::fs::write(&path, body).expect("write playbook");
    path.to_string_lossy().into_owned()
}

#[test]
fn put_of_a_tracking_only_playbook_prints_the_body_and_warns_on_stderr() {
    let server = MockServer::start();
    let saved = server.mock(|when, then| {
        when.method(Method::PUT).path("/v1/playbooks");
        then.status(200)
            .header("content-type", "application/json")
            .body(SAVED);
    });
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_playbook(dir.path(), TRACKING_ONLY);

    let out = vam_put(&server.base_url(), &path);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    saved.assert();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("urn:vam:playbook:local.vam.dev/acme/track"),
        "stdout: {stdout}"
    );
    assert!(
        !stdout.contains("warning"),
        "warnings must not reach stdout: {stdout}"
    );
    assert!(
        stderr.contains("warning: This playbook enforces only step tracking"),
        "stderr: {stderr}"
    );
}

#[test]
fn put_of_the_jira_playbook_prints_no_warning() {
    let server = MockServer::start();
    let saved = server.mock(|when, then| {
        when.method(Method::PUT).path("/v1/playbooks");
        then.status(200)
            .header("content-type", "application/json")
            .body(SAVED);
    });
    let jira = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/jira-assistant/playbook.json"
    );

    let out = vam_put(&server.base_url(), jira);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    saved.assert();
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("warning"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_rejected_put_prints_no_warning() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::PUT).path("/v1/playbooks");
        then.status(422).body(r#"{"error":"invalid"}"#);
    });
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_playbook(dir.path(), TRACKING_ONLY);

    let out = vam_put(&server.base_url(), &path);
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("warning: This playbook"));
}
