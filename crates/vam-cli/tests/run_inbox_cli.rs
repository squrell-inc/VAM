use vam_cli::client::Client;
use vam_cli::cmd::run::{answer_body, discarded_lines, outbox_line, send_body};
use vam_cli::config::ResolvedConfig;

fn client(server: &httpmock::MockServer) -> Client {
    Client::new(&ResolvedConfig {
        host: server.base_url(),
        api_key: "vam_sk_test".into(),
    })
    .unwrap()
}

#[test]
fn send_posts_steer_or_queue_with_idempotency_key() {
    let server = httpmock::MockServer::start();
    let steer = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/runs/run-1/messages")
            .header("Idempotency-Key", "k1")
            .json_body(serde_json::json!({"text":"hi","idempotencyKey":"k1"}));
        then.status(202).body(r#"{"seq":3}"#);
    });
    let queue = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/runs/run-1/messages")
            .header("Idempotency-Key", "k2")
            .json_body(serde_json::json!({"text":"hi","mode":"QUEUE","idempotencyKey":"k2"}));
        then.status(202).body(r#"{"runId":"run-2"}"#);
    });
    let rt = tokio::runtime::Runtime::new().unwrap();
    let c = client(&server);
    rt.block_on(async {
        c.post_raw_json(
            "/v1/runs/run-1/messages",
            &send_body("hi", false, "k1"),
            "k1",
        )
        .await
        .unwrap();
        c.post_raw_json(
            "/v1/runs/run-1/messages",
            &send_body("hi", true, "k2"),
            "k2",
        )
        .await
        .unwrap();
    });
    steer.assert();
    queue.assert();
}

#[test]
fn stop_lists_discarded_text() {
    let body = r#"{"status":{},"discarded":[{"text":"one"},{"text":"two"}]}"#;
    assert_eq!(
        discarded_lines(body),
        vec!["[vam] discarded: one", "[vam] discarded: two"]
    );
    assert!(discarded_lines("{}").is_empty());
}

#[test]
fn answer_submits_or_declines() {
    let v: serde_json::Value =
        serde_json::from_str(&answer_body(Some(r#"{"name":"Ada"}"#), false, "k").unwrap()).unwrap();
    assert_eq!(v["decision"], 4);
    assert_eq!(v["response"]["name"], "Ada");
    let d: serde_json::Value =
        serde_json::from_str(&answer_body(None, true, "k").unwrap()).unwrap();
    assert_eq!(d["decision"], 2);
    assert!(answer_body(Some("[1]"), false, "k").is_err());
    assert!(answer_body(Some("nope"), false, "k").is_err());
    assert!(answer_body(None, false, "k").is_err());
}

#[test]
fn events_print_outbox_messages_and_cards() {
    let m = serde_json::json!({"eventType":"outbox.message","payload":{"text":"Starting"}});
    let c = serde_json::json!({"eventType":"outbox.card","payload":{"jig":"vam.progress","data":{"done":1}}});
    let o = serde_json::json!({"eventType":"tool.invoked","payload":{}});
    assert_eq!(outbox_line(&m).unwrap(), "[vam] agent: Starting");
    assert_eq!(
        outbox_line(&c).unwrap(),
        r#"[vam] card vam.progress: {"done":1}"#
    );
    assert!(outbox_line(&o).is_none());
}
