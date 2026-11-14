//! `vam passwd` against a mock node: the request it sends, what it does with the stored
//! session, and how it reports a refusal.

use httpmock::MockServer;
use vam_cli::client::Client;
use vam_cli::config::{ConfigFile, ResolvedConfig};

/// `vam_cli::output` keeps `--json` mode in a process-wide flag; start every test from human mode.
static PROCESS_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn session_file(host: &str) -> ConfigFile {
    ConfigFile {
        host: Some(host.to_string()),
        session_token: Some("vam_st_login".into()),
        refresh_token: Some("vam_rt_login".into()),
        session_expires_at: Some(9999999999),
        api_key: Some("vam_sk_keep".into()),
        ..Default::default()
    }
}

fn client_for(server: &MockServer) -> Client {
    Client::new(&ResolvedConfig {
        host: server.base_url(),
        api_key: "vam_st_login".into(),
    })
    .unwrap()
}

#[test]
fn success_sends_both_passwords_with_a_key_and_clears_the_stored_session() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/me/password")
            .header("authorization", "Bearer vam_st_login")
            .header_exists("Idempotency-Key")
            .body_contains("\"currentPassword\":\"old-pw\"")
            .body_contains("\"newPassword\":\"new-pw\"");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{"user":{"userId":"alice"},"reloginRequired":true,"passwordUpdatedAt":"1700000000"}"#,
            );
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut file = session_file(&server.base_url());
    rt.block_on(async {
        vam_cli::cmd::passwd::change_and_clear_session(
            &client_for(&server),
            "old-pw",
            "new-pw",
            &mut file,
        )
        .await
        .unwrap();
    });
    mock.assert();
    file.save_to(&path).unwrap();
    let saved = ConfigFile::load_from(&path).unwrap();
    assert!(saved.session_token.is_none());
    assert!(saved.refresh_token.is_none());
    assert!(saved.session_expires_at.is_none());
    assert_eq!(saved.api_key.as_deref(), Some("vam_sk_keep"));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("old-pw") && !text.contains("new-pw"));
}

#[test]
fn each_call_sends_a_fresh_idempotency_key() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/v1/me/password");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"reloginRequired":true}"#);
    });
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let c = client_for(&server);
        vam_cli::cmd::passwd::change_password(&c, "a", "b")
            .await
            .unwrap();
        vam_cli::cmd::passwd::change_password(&c, "a", "b")
            .await
            .unwrap();
    });
    mock.assert_hits(2);
}

#[test]
fn refusals_keep_the_session_and_report_what_the_node_said() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    vam_cli::output::set_json(false);
    let server = MockServer::start();
    let cases: [(u16, &str, &str); 3] = [
        (
            403,
            r#"{"code":"PermissionDenied","message":"current password is incorrect","retryable":false}"#,
            "current password is incorrect",
        ),
        (
            403,
            r#"{"code":"PermissionDenied","message":"changing a password needs a password session (vam login); API keys cannot change a password","retryable":false}"#,
            "API keys cannot change a password",
        ),
        (
            429,
            r#"{"code":"RESOURCE_EXHAUSTED","message":"too many wrong password attempts; retry after 42s","retryable":true}"#,
            "retry after 42s",
        ),
    ];
    let rt = tokio::runtime::Runtime::new().unwrap();
    for (status, body, expect) in cases {
        let mut mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v1/me/password");
            then.status(status)
                .header("content-type", "application/json")
                .body(body);
        });
        let mut file = session_file(&server.base_url());
        let err = rt.block_on(async {
            vam_cli::cmd::passwd::change_and_clear_session(
                &client_for(&server),
                "old-pw",
                "new-pw",
                &mut file,
            )
            .await
            .unwrap_err()
        });
        mock.assert();
        mock.delete();
        let text = err.to_string();
        assert!(text.contains(expect), "{text}");
        assert!(
            !text.contains("old-pw") && !text.contains("new-pw"),
            "{text}"
        );
        assert_eq!(file.session_token.as_deref(), Some("vam_st_login"));
        assert_eq!(file.refresh_token.as_deref(), Some("vam_rt_login"));
        assert_eq!(vam_cli::output::classify(&err).1, Some(status));
    }
}
