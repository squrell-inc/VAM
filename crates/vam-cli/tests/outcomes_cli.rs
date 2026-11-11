use std::process::{Command, Output};

use httpmock::{Method, MockServer};

const URN: &str = "urn:vam:agent:local.vam.dev/acme/stats";

const AGGREGATE: &str = r#"{"version":"1.0.0","runs":4,"successes":3,"failed":1,"timedOut":0,
 "successRate":0.75,"approvalsPerRun":0.5,"tokensPerSuccess":120.0,
 "passK":{"k":2,"value":0.5},"costPerSuccessMicroUsd":900,
 "effects":{"journaledRuns":4,"unjournaledRuns":0,"confirmed":3,"unknown":0,"duplicates":0,"identicalRepeats":0}}"#;

const DIFF: &str = r#"{
 "base":{"version":"1.0.0","runs":4,"successes":3},
 "candidate":{"version":"draft","runs":2,"successes":2},
 "changes":{
  "successRate":{"base":0.75,"candidate":1.0,"delta":0.25},
  "tokensPerSuccess":{"base":120.0,"candidate":100.0,"delta":-20.0},
  "duplicates":{"base":0,"candidate":0,"delta":0}}}"#;

const DIFF_TEXT: &str = "draft vs 1.0.0 (2 runs vs 4 runs)\n  success rate          0.750 -> 1 (+0.250)\n  tokens per success    120 -> 100 (-20)\n  duplicates            0 -> 0\n";

fn path(suffix: &str) -> String {
    format!(
        "/v1/agents/{}{suffix}",
        vam_cli::client::encode_path_segment(URN)
    )
}

fn vam(server: &MockServer, args: &[&str]) -> Output {
    let dir = tempfile::tempdir().expect("tempdir");
    Command::new(env!("CARGO_BIN_EXE_vam"))
        .current_dir(dir.path())
        .env_remove("VAM_API_KEY")
        .env_remove("VAM_HOST")
        .args(["--host", &server.base_url(), "--token", "vam_sk_test"])
        .args(args)
        .output()
        .expect("spawn vam")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn stats_prints_one_version_as_text() {
    let server = MockServer::start();
    let m = server.mock(|when, then| {
        when.method(Method::GET)
            .path(path("/outcomes"))
            .query_param("version", "1.0.0")
            .query_param("k", "2");
        then.status(200)
            .header("content-type", "application/json")
            .body(AGGREGATE);
    });
    let out = vam(
        &server,
        &["agent", "stats", URN, "--version", "1.0.0", "--k", "2"],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    m.assert();
    assert_eq!(
        text(&out.stdout),
        "version 1.0.0\n  runs                  4 (3 succeeded, 1 failed, 0 timed out)\n  success rate          0.750\n  approvals per run     0.500\n  tokens per success    120\n  pass^2                0.500\n  cost per success      900 micro-USD\n  effects               4 journaled, 3 confirmed, 0 unknown, 0 duplicates\n"
    );
}

#[test]
fn stats_diff_prints_the_changes_and_json_passes_through() {
    let server = MockServer::start();
    let m = server.mock(|when, then| {
        when.method(Method::GET)
            .path(path("/outcomes/diff"))
            .query_param("version", "draft");
        then.status(200)
            .header("content-type", "application/json")
            .body(DIFF);
    });
    let out = vam(&server, &["agent", "stats", URN, "--diff"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), DIFF_TEXT);
    let out = vam(&server, &["agent", "stats", URN, "--diff", "--json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["base"]["version"], "1.0.0");
    m.assert_hits(2);
}

fn release_mock(server: &MockServer) -> httpmock::Mock<'_> {
    server.mock(|when, then| {
        when.method(Method::POST).path(path("/release"));
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(r#"{{"resourceId":{{"urn":"{URN}"}}}}"#));
    })
}

#[test]
fn release_prints_the_diff_on_stderr_and_leaves_stdout_the_release_json() {
    let server = MockServer::start();
    let d = server.mock(|when, then| {
        when.method(Method::GET)
            .path(path("/outcomes/diff"))
            .query_param("version", "draft");
        then.status(200)
            .header("content-type", "application/json")
            .body(DIFF);
    });
    let r = release_mock(&server);
    let out = vam(
        &server,
        &["--json", "agent", "release", URN, "--version", "1.1.0"],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    d.assert();
    r.assert();
    assert!(
        text(&out.stderr).starts_with(DIFF_TEXT),
        "{}",
        text(&out.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(stdout, serde_json::json!({"resourceId": {"urn": URN}}));
    assert!(!text(&out.stdout).contains("draft vs"));
}

#[test]
fn release_still_releases_when_the_diff_cannot_be_fetched() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET).path(path("/outcomes/diff"));
        then.status(500).body("boom");
    });
    let r = release_mock(&server);
    let out = vam(
        &server,
        &["--json", "agent", "release", URN, "--version", "1.1.0"],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    r.assert();
    assert!(
        text(&out.stderr).contains("warning: could not fetch the outcomes diff"),
        "{}",
        text(&out.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(stdout["resourceId"]["urn"], URN);
}
