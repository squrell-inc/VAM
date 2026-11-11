//! The broker end to end against the fake transport and a mock notification gateway: delivery,
//! idempotency, updates, the link relay, the click relay, the app token path, and that nothing
//! secret lands in the state directory.
#![cfg(feature = "fake-transport")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Channel, Endpoint, Server, Uri};
use vam_notify::adapter::{InboundStream, Registry};
use vam_notify::control::{self, Request};
use vam_notify::gateway::GatewayClient;
use vam_notify::hub::Hub;
use vam_notify::log::DeliveryLog;
use vam_notify::serve::{bind_unix_listener, serve_unix};
use vam_notify::slack::SlackAdapter;
use vam_notify::state::StateLock;
use vam_notify::transport::fake::{FakeTransport, SENT_FILE};
use vam_notify::transport::{SlackTransport, TransportError};
use vam_notify::NotifyBroker;
use vam_spec::v1::hosted_notification_channel_client::HostedNotificationChannelClient;
use vam_spec::v1::hosted_notification_gateway_server::{
    HostedNotificationGateway, HostedNotificationGatewayServer,
};
use vam_spec::v1::{
    ActOnNotificationRequest, ActOnNotificationResponse, ActOnOutcome, ActionStyle,
    DeliverNotificationRequest, DeliverOutcome, GetChannelConfigRequest, GetChannelConfigResponse,
    LinkChannelAccountRequest, LinkChannelAccountResponse, LinkOutcome, NotificationRendering,
    RenderedAction,
};

const BOT: &str = "xoxb-BOTSECRET-1234";
const APP: &str = "xapp-1-APPSECRET-5678";
const HANDLE: &str = "vam_na_HANDLE1.SECRETPART";
const CODE: &str = "ABCDE-FGHJK";

#[derive(Default)]
struct Calls {
    configs: u32,
    links: Vec<LinkChannelAccountRequest>,
    acts: Vec<ActOnNotificationRequest>,
}

struct MockGateway {
    calls: Arc<Mutex<Calls>>,
}

#[tonic::async_trait]
impl HostedNotificationGateway for MockGateway {
    async fn act_on_notification(
        &self,
        request: tonic::Request<ActOnNotificationRequest>,
    ) -> Result<tonic::Response<ActOnNotificationResponse>, tonic::Status> {
        self.calls.lock().unwrap().acts.push(request.into_inner());
        Ok(tonic::Response::new(ActOnNotificationResponse {
            outcome: ActOnOutcome::Unavailable as i32,
            ..Default::default()
        }))
    }

    async fn link_channel_account(
        &self,
        request: tonic::Request<LinkChannelAccountRequest>,
    ) -> Result<tonic::Response<LinkChannelAccountResponse>, tonic::Status> {
        self.calls.lock().unwrap().links.push(request.into_inner());
        Ok(tonic::Response::new(LinkChannelAccountResponse {
            outcome: LinkOutcome::Linked as i32,
            contact_id: "ct_1".into(),
        }))
    }

    async fn get_channel_config(
        &self,
        _request: tonic::Request<GetChannelConfigRequest>,
    ) -> Result<tonic::Response<GetChannelConfigResponse>, tonic::Status> {
        self.calls.lock().unwrap().configs += 1;
        Ok(tonic::Response::new(GetChannelConfigResponse {
            enabled: true,
            settings: Default::default(),
            resolved_secrets: [("app_token".to_string(), APP.to_string())].into(),
        }))
    }
}

async fn dial(path: PathBuf) -> Channel {
    Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let p = path.clone();
            async move { tokio::net::UnixStream::connect(p).await }
        }))
        .await
        .unwrap()
}

struct Rig {
    _tmp: tempfile::TempDir,
    _lock: StateLock,
    dir: PathBuf,
    calls: Arc<Mutex<Calls>>,
    hub: Arc<Hub>,
    hsp: HostedNotificationChannelClient<Channel>,
}

async fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let lock = StateLock::open(&tmp.path().join("state")).unwrap();
    let dir = lock.dir().to_path_buf();

    let calls = Arc::new(Mutex::new(Calls::default()));
    let gw_sock = tmp.path().join("gw.sock");
    let gw_listener = bind_unix_listener(&gw_sock).unwrap();
    let mock = MockGateway {
        calls: calls.clone(),
    };
    tokio::spawn(
        Server::builder()
            .add_service(HostedNotificationGatewayServer::new(mock))
            .serve_with_incoming(UnixListenerStream::new(gw_listener)),
    );

    let registry = Registry::default().with(Arc::new(SlackAdapter::new(Arc::new(
        FakeTransport::new(&dir),
    ))));
    let hub = Hub::new(
        registry,
        DeliveryLog::open(&lock.db_path()).unwrap(),
        GatewayClient::new(Some(gw_sock)),
    );
    let ctl = control::bind(&dir).unwrap();
    tokio::spawn(control::serve(hub.clone(), ctl));
    tokio::spawn(hub.clone().run_connection("slack"));

    let sock = tmp.path().join("n.sock");
    let listener = bind_unix_listener(&sock).unwrap();
    tokio::spawn(serve_unix(listener, NotifyBroker::new(hub.clone())));
    let hsp = HostedNotificationChannelClient::new(dial(sock).await);
    Rig {
        _tmp: tmp,
        _lock: lock,
        dir,
        calls,
        hub,
        hsp,
    }
}

fn action(id: &str, label: &str, decision: &str, style: ActionStyle) -> RenderedAction {
    RenderedAction {
        id: id.into(),
        label: label.into(),
        decision: decision.into(),
        option: String::new(),
        style: style as i32,
    }
}

fn approval(id: &str, handle: &str) -> DeliverNotificationRequest {
    DeliverNotificationRequest {
        delivery_id: id.into(),
        channel: "slack".into(),
        channel_address: "T1/U1".into(),
        notification_id: "nt_1".into(),
        kind: "approval.pending".into(),
        rendering: Some(NotificationRendering {
            title: "Approve write_file?".into(),
            body: "A run waits for you.".into(),
            actions: vec![
                action("approve", "Approve", "APPROVED", ActionStyle::Primary),
                action("deny", "Deny", "REJECTED", ActionStyle::Danger),
            ],
            open_url: "https://vam.example.test/notifications/nt_1".into(),
            cli_line: "vam approve decide ap1".into(),
            ..Default::default()
        }),
        action_handle: handle.into(),
        update_of: String::new(),
        resolved_secrets: [("bot_token".to_string(), BOT.to_string())].into(),
        trace_context: None,
    }
}

fn update(id: &str, of: &str) -> DeliverNotificationRequest {
    let mut r = approval(id, "");
    r.update_of = of.into();
    r.rendering = Some(NotificationRendering {
        title: "Decided by Ann in app at 2026-11-04 10:00 UTC".into(),
        body: "Approve write_file?".into(),
        open_url: "https://vam.example.test/notifications/nt_1".into(),
        ..Default::default()
    });
    r
}

fn sent(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join(SENT_FILE))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn of_method<'a>(lines: &'a [Value], m: &str) -> Vec<&'a Value> {
    lines.iter().filter(|l| l["method"] == m).collect()
}

fn buttons(v: &Value) -> usize {
    match v {
        Value::Object(m) => {
            usize::from(m.get("type").and_then(Value::as_str) == Some("button"))
                + m.values().map(buttons).sum::<usize>()
        }
        Value::Array(a) => a.iter().map(buttons).sum(),
        _ => 0,
    }
}

/// Every file under `dir` that holds any of `needles`.
fn files_holding(dir: &Path, needles: &[&str]) -> Vec<String> {
    let mut hits = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for ent in std::fs::read_dir(&d).unwrap().flatten() {
            let p = ent.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            for n in needles {
                if text.contains(n) {
                    hits.push(format!("{} holds {n}", p.display()));
                }
            }
        }
    }
    hits
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..60 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never happened: {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delivery_posts_once_and_no_secret_reaches_the_state_dir() {
    let mut r = rig().await;
    let first = r
        .hsp
        .deliver_notification(approval("nd_1", HANDLE))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.outcome, DeliverOutcome::Sent as i32, "{first:?}");
    let again = r
        .hsp
        .deliver_notification(approval("nd_1", HANDLE))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(again.outcome, DeliverOutcome::Duplicate as i32);

    let lines = sent(&r.dir);
    let posts = of_method(&lines, "chat.postMessage");
    assert_eq!(posts.len(), 1, "a repeated delivery_id posts once");
    let body = &posts[0]["body"];
    assert_eq!(body["channel"], "U1");
    assert_eq!(body["metadata"]["event_type"], "vam.approval.pending");
    assert_eq!(buttons(&body["blocks"]), 2);
    assert!(body.to_string().contains("Open in VAM"));
    assert!(lines.iter().all(|l| l["at_ms"].as_u64().is_some()));

    let row = r.hub.log().get("nd_1").unwrap().unwrap();
    assert!(row.is_sent() && row.attempts == 1 && row.last_vendor_status == "ok");
    assert_eq!(row.notification_id, "nt_1");

    // The app token path: GetChannelConfig is called at start, whatever the transport.
    eventually("GetChannelConfig at start", || {
        r.calls.lock().unwrap().configs >= 1
    })
    .await;

    // A DM with a code is relayed and the outcome is told; the code is nowhere afterwards.
    let dm = control::call(
        &r.dir,
        &Request::Dm {
            account: "T1/U1".into(),
            text: CODE.into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(dm["status"], "LINKED");
    {
        let calls = r.calls.lock().unwrap();
        assert_eq!(calls.links.len(), 1);
        assert_eq!(calls.links[0].code, CODE);
        let who = calls.links[0].external_account.clone().unwrap();
        assert_eq!((who.scope.as_str(), who.id.as_str()), ("T1", "U1"));
    }
    let lines = sent(&r.dir);
    let reply = of_method(&lines, "chat.postMessage")
        .into_iter()
        .find(|l| {
            l["body"]["text"]
                .as_str()
                .is_some_and(|t| t.starts_with("Linked"))
        })
        .expect("the outcome is DMed");
    assert!(reply["body"].get("metadata").is_none());

    // A DM that is not a code is not relayed.
    let ignored = control::call(
        &r.dir,
        &Request::Dm {
            account: "T1/U1".into(),
            text: "hello there".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(ignored["status"], "IGNORED");
    assert_eq!(r.calls.lock().unwrap().links.len(), 1);

    // Nothing in the state dir: bot token, app token, handle, link code.
    let hits = files_holding(
        &r.dir,
        &[
            BOT,
            APP,
            "xoxb-",
            "xapp-",
            HANDLE,
            "SECRETPART",
            "vam_na_",
            CODE,
            "ABCDE",
        ],
    );
    assert!(hits.is_empty(), "{hits:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_edits_the_original_message_and_drops_its_buttons() {
    let mut r = rig().await;
    r.hsp
        .deliver_notification(approval("nd_1", HANDLE))
        .await
        .unwrap();
    let ts = r.hub.log().get("nd_1").unwrap().unwrap().message_ts;

    let out = r
        .hsp
        .deliver_notification(update("nd_2", "nd_1"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(out.outcome, DeliverOutcome::Sent as i32, "{out:?}");
    let lines = sent(&r.dir);
    let edits = of_method(&lines, "chat.update");
    assert_eq!(edits.len(), 1);
    let b = &edits[0]["body"];
    assert_eq!(b["ts"], json!(ts));
    assert_eq!(b["channel"], "U1");
    assert_eq!(b["metadata"]["event_type"], "vam.approval.pending");
    assert_eq!(buttons(&b["blocks"]), 0);
    assert!(b["text"].as_str().unwrap().contains("app"));
    assert_eq!(
        of_method(&lines, "chat.postMessage").len(),
        1,
        "an update posts nothing new"
    );

    let dup = r
        .hsp
        .deliver_notification(update("nd_2", "nd_1"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(dup.outcome, DeliverOutcome::Duplicate as i32);
    assert_eq!(of_method(&sent(&r.dir), "chat.update").len(), 1);

    let missing = r
        .hsp
        .deliver_notification(update("nd_3", "nd_unknown"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(missing.outcome, DeliverOutcome::PermanentFailure as i32);
    assert_eq!(missing.error_code, "original_not_found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_click_relays_the_held_handle_and_the_person_is_told_when_vamd_cannot_act() {
    let mut r = rig().await;
    r.hsp
        .deliver_notification(approval("nd_1", HANDLE))
        .await
        .unwrap();
    let click = |action: &str, handle: Option<&str>| Request::Click {
        account: "T1/U1".into(),
        message: "approval.pending".into(),
        action: action.into(),
        handle: handle.map(String::from),
        gateway_socket: None,
    };

    let a = control::call(&r.dir, &click("approve", None))
        .await
        .unwrap();
    assert_eq!(a["status"], "UNAVAILABLE");
    {
        let calls = r.calls.lock().unwrap();
        assert_eq!(calls.acts.len(), 1);
        let act = &calls.acts[0];
        assert_eq!(act.handle, HANDLE);
        assert_eq!(act.decision, "APPROVED");
        let who = act.external_account.clone().unwrap();
        assert_eq!((who.scope.as_str(), who.id.as_str()), ("T1", "U1"));
    }
    let lines = sent(&r.dir);
    let eph = of_method(&lines, "chat.postEphemeral");
    assert_eq!(eph.len(), 1);
    assert!(eph[0]["body"]["text"]
        .as_str()
        .unwrap()
        .contains("Try again in the app"));

    // An action the message never offered is refused here and never reaches vamd.
    let bad = control::call(&r.dir, &click("submit", None)).await.unwrap();
    assert_eq!(bad["status"], "INVALID");
    assert_eq!(r.calls.lock().unwrap().acts.len(), 1);

    // Forged and replayed handles are passed through for vamd to judge.
    control::call(&r.dir, &click("deny", Some("forged")))
        .await
        .unwrap();
    control::call(&r.dir, &click("deny", Some("replay")))
        .await
        .unwrap();
    {
        let calls = r.calls.lock().unwrap();
        assert!(calls.acts[1].handle.starts_with("vam_na_forged"));
        assert_eq!(calls.acts[2].handle, calls.acts[1].handle);
        assert_eq!(calls.acts[1].decision, "REJECTED");
    }
    // A handle of an account that was sent nothing cannot be borrowed.
    assert!(control::call(&r.dir, &click("approve", Some("from:T1/U2")))
        .await
        .is_err());

    // The listing names deliveries, never handle values.
    let listing = control::call(&r.dir, &Request::Handles).await.unwrap();
    let text = listing["handles"].to_string();
    assert!(text.contains("nd_1") && text.contains("approve"));
    assert!(!text.contains("SECRETPART") && !text.contains("vam_na_"));
}

/// A transport that answers from a script.
struct Scripted {
    answers: Mutex<Vec<Result<Value, TransportError>>>,
}

#[async_trait]
impl SlackTransport for Scripted {
    async fn call(&self, method: &str, body: Value, _t: &str) -> Result<Value, TransportError> {
        let mut a = self.answers.lock().unwrap();
        if a.is_empty() {
            return Ok(match method {
                "conversations.open" => json!({ "ok": true, "channel": { "id": "D1" } }),
                _ => json!({ "ok": true, "channel": body["channel"], "ts": "1700.000009" }),
            });
        }
        a.remove(0)
    }
    async fn open_socket(&self, _t: &str) -> Result<InboundStream, TransportError> {
        Err(TransportError::Unavailable("none".into()))
    }
}

#[tokio::test]
async fn vendor_trouble_is_retry_or_permanent_and_a_retry_then_sends() {
    let tmp = tempfile::tempdir().unwrap();
    let scripted = Arc::new(Scripted {
        answers: Mutex::new(vec![
            Err(TransportError::RateLimited {
                retry_after_ms: 7000,
            }),
            Err(TransportError::Server(502)),
            Err(TransportError::Api("user_not_found".into())),
        ]),
    });
    let registry = Registry::default().with(Arc::new(SlackAdapter::new(scripted)));
    let hub = Hub::new(
        registry,
        DeliveryLog::open(&tmp.path().join("d.db")).unwrap(),
        GatewayClient::new(None),
    );
    let outcome =
        |r: vam_spec::v1::DeliverNotificationResponse| (r.outcome, r.error_code, r.retry_after_ms);

    let r1 = hub.deliver(approval("nd_1", "")).await;
    assert_eq!(
        outcome(r1),
        (DeliverOutcome::Retry as i32, "ratelimited".into(), 7000)
    );
    let r2 = hub.deliver(approval("nd_1", "")).await;
    assert_eq!(outcome(r2).1, "slack_server_error");
    let r3 = hub.deliver(approval("nd_1", "")).await;
    assert_eq!(
        outcome(r3),
        (
            DeliverOutcome::PermanentFailure as i32,
            "user_not_found".into(),
            0
        )
    );
    // vamd gave up on none of these by itself; a later try goes through and the log counts them.
    let r4 = hub.deliver(approval("nd_1", "")).await;
    assert_eq!(r4.outcome, DeliverOutcome::Sent as i32, "{r4:?}");
    let row = hub.log().get("nd_1").unwrap().unwrap();
    assert_eq!((row.attempts, row.is_sent()), (4, true));
    assert_eq!(row.slack_channel_id, "D1");
}

#[tokio::test]
async fn a_missing_bot_token_and_a_secret_shaped_id_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = Registry::default().with(Arc::new(SlackAdapter::new(Arc::new(Scripted {
        answers: Mutex::new(vec![]),
    }))));
    let hub = Hub::new(
        registry,
        DeliveryLog::open(&tmp.path().join("d.db")).unwrap(),
        GatewayClient::new(None),
    );
    let mut no_token = approval("nd_1", "");
    no_token.resolved_secrets.clear();
    let r = hub.deliver(no_token).await;
    assert_eq!(
        (r.outcome, r.error_code.as_str()),
        (DeliverOutcome::PermanentFailure as i32, "no_token")
    );
    let r = hub.deliver(approval("xoxb-not-an-id", "")).await;
    assert_eq!(
        (r.outcome, r.error_code.as_str()),
        (DeliverOutcome::PermanentFailure as i32, "invalid_request")
    );
}
