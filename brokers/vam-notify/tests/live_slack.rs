//! The live Slack transport against local fake servers only: a hand-rolled HTTP server for the
//! Web API and a WebSocket server for Socket Mode, both on 127.0.0.1 with the base URL injected
//! through `LiveTransport::for_test`. Nothing here contacts Slack.
#![cfg(feature = "fake-transport")]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_tungstenite::tungstenite::Message;
use tonic::transport::{Channel, Endpoint, Server, Uri};
use vam_notify::adapter::{
    Binding, ChannelAdapter, Credentials, ExternalRef, InboundEvent, Registry,
};
use vam_notify::gateway::GatewayClient;
use vam_notify::hub::Hub;
use vam_notify::log::DeliveryLog;
use vam_notify::serve::{bind_unix_listener, serve_unix};
use vam_notify::slack::SlackAdapter;
use vam_notify::state::StateLock;
use vam_notify::transport::{LiveTransport, SlackTransport, TransportError};
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

const BOT: &str = "xoxb-LIVEBOT-SECRET-7788";
const APP_PREFIX: &str = "xapp-1-LIVEAPP-SECRET-";
const TICKET: &str = "TICKETSECRET5566";
const HANDLE: &str = "vam_na_LIVEHANDLE.SECRETPART";

// ---- a fake Web API ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Req {
    path: String,
    auth: String,
    body: Value,
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Resp {
    fn json(v: Value) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: v.to_string(),
        }
    }
}

type Handler = Arc<dyn Fn(&Req) -> Resp + Send + Sync>;

struct Web {
    addr: SocketAddr,
    reqs: Arc<Mutex<Vec<Req>>>,
}

impl Web {
    fn base(&self) -> String {
        format!("http://{}/api", self.addr)
    }
    fn reqs(&self) -> Vec<Req> {
        self.reqs.lock().unwrap().clone()
    }
    fn of(&self, method: &str) -> Vec<Req> {
        self.reqs()
            .into_iter()
            .filter(|r| r.path == format!("/api/{method}"))
            .collect()
    }
}

async fn read_request(sock: &mut TcpStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let path = lines.next()?.split_whitespace().nth(1)?.to_string();
    let mut auth = String::new();
    let mut len = 0usize;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "authorization" => auth = v.trim().to_string(),
                "content-length" => len = v.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Req {
        path,
        auth,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

async fn web(handler: Handler) -> Web {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let reqs = Arc::new(Mutex::new(Vec::new()));
    let seen = reqs.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let handler = handler.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let Some(req) = read_request(&mut sock).await else {
                    return;
                };
                let resp = handler(&req);
                seen.lock().unwrap().push(req);
                let mut out = format!(
                    "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    resp.status,
                    resp.body.len()
                );
                for (k, v) in &resp.headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                out.push_str(&resp.body);
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    Web { addr, reqs }
}

/// The happy Web API: DM channel `D1`, ts `1.0001`, and `apps.connections.open` handing out the
/// socket URL (with a ticket in it, like Slack's).
fn happy(ws: SocketAddr) -> Handler {
    Arc::new(move |r| match r.path.as_str() {
        "/api/conversations.open" => Resp::json(json!({ "ok": true, "channel": { "id": "D1" } })),
        "/api/chat.postMessage" => {
            Resp::json(json!({ "ok": true, "channel": "D1", "ts": "1.0001" }))
        }
        "/api/chat.update" => Resp::json(json!({ "ok": true, "channel": "D1", "ts": "1.0001" })),
        "/api/chat.postEphemeral" => Resp::json(json!({ "ok": true })),
        "/api/apps.connections.open" => {
            Resp::json(json!({ "ok": true, "url": format!("ws://{ws}/link/?ticket={TICKET}") }))
        }
        _ => Resp::json(json!({ "ok": false, "error": "unknown_method" })),
    })
}

// ---- a fake Socket Mode server ----------------------------------------------------------------

struct Ws {
    addr: SocketAddr,
    conns: Arc<AtomicUsize>,
    acks: Arc<Mutex<Vec<Value>>>,
    out: mpsc::UnboundedSender<Value>,
}

impl Ws {
    /// Sends one envelope on the live connection.
    fn send(&self, v: Value) {
        self.out.send(v).unwrap();
    }
}

async fn ws_server() -> Ws {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let conns = Arc::new(AtomicUsize::new(0));
    let acks = Arc::new(Mutex::new(Vec::new()));
    let (out, rx) = mpsc::unbounded_channel::<Value>();
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let (c, a) = (conns.clone(), acks.clone());
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await else {
                continue;
            };
            c.fetch_add(1, Ordering::SeqCst);
            let (rx, a) = (rx.clone(), a.clone());
            tokio::spawn(async move {
                let hello = json!({ "type": "hello", "num_connections": 1 });
                let _ = ws.send(Message::Text(hello.to_string())).await;
                let mut rx = rx.lock().await;
                loop {
                    tokio::select! {
                        Some(v) = rx.recv() => {
                            let disconnect = v["type"] == "disconnect";
                            if ws.send(Message::Text(v.to_string())).await.is_err() {
                                return;
                            }
                            if disconnect {
                                // Slack closes the socket after a disconnect envelope.
                                let _ = ws.close(None).await;
                                return;
                            }
                        }
                        m = ws.next() => match m {
                            Some(Ok(Message::Text(t))) => {
                                a.lock().unwrap().push(serde_json::from_str(&t).unwrap());
                            }
                            Some(Ok(_)) => {}
                            _ => return,
                        }
                    }
                }
            });
        }
    });
    Ws {
        addr,
        conns,
        acks,
        out,
    }
}

// ---- a mock gateway ---------------------------------------------------------------------------

#[derive(Default)]
struct Calls {
    connect_configs: u32,
    reply_configs: u32,
    acts: Vec<ActOnNotificationRequest>,
    links: Vec<LinkChannelAccountRequest>,
}

struct MockGateway {
    calls: Arc<Mutex<Calls>>,
    pin: Option<String>,
}

#[tonic::async_trait]
impl HostedNotificationGateway for MockGateway {
    async fn act_on_notification(
        &self,
        request: tonic::Request<ActOnNotificationRequest>,
    ) -> Result<tonic::Response<ActOnNotificationResponse>, tonic::Status> {
        self.calls.lock().unwrap().acts.push(request.into_inner());
        Ok(tonic::Response::new(ActOnNotificationResponse {
            outcome: ActOnOutcome::Accepted as i32,
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
        request: tonic::Request<GetChannelConfigRequest>,
    ) -> Result<tonic::Response<GetChannelConfigResponse>, tonic::Status> {
        let mut c = self.calls.lock().unwrap();
        let mut secrets = std::collections::HashMap::new();
        if request.into_inner().purpose == "connect" {
            c.connect_configs += 1;
            // A different app token on each connect, so a reused one shows.
            secrets.insert(
                "app_token".to_string(),
                format!("{APP_PREFIX}{}", c.connect_configs),
            );
        } else {
            c.reply_configs += 1;
            secrets.insert("bot_token".to_string(), BOT.to_string());
        }
        let mut settings = std::collections::HashMap::new();
        if let Some(p) = &self.pin {
            settings.insert("pinned_workspace".to_string(), p.clone());
        }
        Ok(tonic::Response::new(GetChannelConfigResponse {
            enabled: true,
            settings,
            resolved_secrets: secrets,
        }))
    }
}

// ---- tracing capture --------------------------------------------------------------------------

static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

struct Sink(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Installs (once per process) a subscriber at TRACE for every target, into a buffer.
fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    LOGS.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let b = buf.clone();
        let sub = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || Sink(b.clone()))
            .finish();
        let _ = tracing::subscriber::set_global_default(sub);
        buf
    })
    .clone()
}

fn logs_text() -> String {
    String::from_utf8_lossy(&captured_logs().lock().unwrap()).to_string()
}

// ---- helpers ----------------------------------------------------------------------------------

fn creds() -> Credentials {
    Credentials::new([("bot_token".to_string(), BOT.to_string())].into())
}

fn binding() -> Binding {
    Binding {
        address: "T1/U1".into(),
        delivery_id: "nd_1".into(),
        notification_id: "nt_1".into(),
        kind: "approval.pending".into(),
        update_of: None,
    }
}

fn rendering() -> NotificationRendering {
    NotificationRendering {
        title: "Approve write_file?".into(),
        body: "A run waits for you.".into(),
        actions: vec![
            action("approve", "Approve", "APPROVED", ActionStyle::Primary),
            action("deny", "Deny", "REJECTED", ActionStyle::Danger),
        ],
        open_url: "https://vam.example.test/notifications/nt_1".into(),
        cli_line: "vam approve decide ap1".into(),
        ..Default::default()
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

fn adapter(base: &str) -> SlackAdapter {
    SlackAdapter::new(Arc::new(LiveTransport::for_test(base)))
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never happened: {what}");
}

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
            let text = String::from_utf8_lossy(&std::fs::read(&p).unwrap_or_default()).to_string();
            for n in needles {
                if text.contains(n) {
                    hits.push(format!("{} holds {n}", p.display()));
                }
            }
        }
    }
    hits
}

// ---- Web API tests ----------------------------------------------------------------------------

#[tokio::test]
async fn a_post_and_an_update_round_trip_with_the_bearer_token() {
    let w = web(happy("127.0.0.1:1".parse().unwrap())).await;
    let a = adapter(&w.base());
    let sent = a.deliver(&rendering(), &binding(), &creds()).await;
    assert_eq!(sent.outcome, DeliverOutcome::Sent, "{sent:?}");
    let ext = sent.external.clone().unwrap();
    assert_eq!((ext.channel_id.as_str(), ext.ts.as_str()), ("D1", "1.0001"));

    let mut upd = binding();
    upd.delivery_id = "nd_2".into();
    upd.update_of = Some(ExternalRef {
        channel_id: ext.channel_id,
        ts: ext.ts,
    });
    let mut r = rendering();
    r.actions.clear();
    let done = a.deliver(&r, &upd, &creds()).await;
    assert_eq!(done.outcome, DeliverOutcome::Sent, "{done:?}");

    let reqs = w.reqs();
    let methods: Vec<_> = reqs.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        methods,
        [
            "/api/conversations.open",
            "/api/chat.postMessage",
            "/api/chat.update"
        ]
    );
    assert!(reqs.iter().all(|r| r.auth == format!("Bearer {BOT}")));
    assert_eq!(reqs[0].body["users"], "U1");
    assert_eq!(reqs[1].body["channel"], "D1");
    assert_eq!(reqs[2].body["ts"], "1.0001");
    assert_eq!(reqs[2].body["channel"], "D1");
    // The token is in the header only, never in a body.
    assert!(reqs.iter().all(|r| !r.body.to_string().contains(BOT)));
}

#[tokio::test]
async fn a_429_with_retry_after_becomes_a_retry_with_the_wait() {
    let w = web(Arc::new(|_| Resp {
        status: 429,
        headers: vec![("Retry-After".into(), "7".into())],
        body: String::new(),
    }))
    .await;
    let t = LiveTransport::for_test(&w.base());
    match t.call("chat.postMessage", json!({}), BOT).await {
        Err(TransportError::RateLimited { retry_after_ms }) => assert_eq!(retry_after_ms, 7000),
        other => panic!("{other:?}"),
    }
    let r = adapter(&w.base())
        .deliver(&rendering(), &binding(), &creds())
        .await;
    assert_eq!(r.outcome, DeliverOutcome::Retry);
    assert_eq!(r.retry_after_ms, 7000);
    assert_eq!(r.error_code, "ratelimited");
}

#[tokio::test]
async fn a_server_error_is_a_retry() {
    let w = web(Arc::new(|_| Resp {
        status: 503,
        headers: vec![],
        body: String::new(),
    }))
    .await;
    let r = adapter(&w.base())
        .deliver(&rendering(), &binding(), &creds())
        .await;
    assert_eq!(r.outcome, DeliverOutcome::Retry);
    assert_eq!(r.error_code, "slack_server_error");
}

#[tokio::test]
async fn invalid_auth_is_permanent_and_user_not_found_too() {
    let w = web(Arc::new(|_| {
        Resp::json(json!({ "ok": false, "error": "invalid_auth" }))
    }))
    .await;
    let r = adapter(&w.base())
        .deliver(&rendering(), &binding(), &creds())
        .await;
    assert_eq!(r.outcome, DeliverOutcome::PermanentFailure, "{r:?}");
    assert_eq!(r.error_code, "invalid_auth");

    let w = web(Arc::new(|_| {
        Resp::json(json!({ "ok": false, "error": "user_not_found" }))
    }))
    .await;
    let r = adapter(&w.base())
        .deliver(&rendering(), &binding(), &creds())
        .await;
    assert_eq!(r.outcome, DeliverOutcome::PermanentFailure);
    assert_eq!(r.error_code, "user_not_found");
}

#[tokio::test]
async fn ok_false_ratelimited_is_a_rate_limit() {
    let w = web(Arc::new(|_| Resp {
        status: 200,
        headers: vec![("Retry-After".into(), "2".into())],
        body: json!({ "ok": false, "error": "ratelimited" }).to_string(),
    }))
    .await;
    let t = LiveTransport::for_test(&w.base());
    assert!(matches!(
        t.call("chat.postMessage", json!({}), BOT).await,
        Err(TransportError::RateLimited {
            retry_after_ms: 2000
        })
    ));
}

#[tokio::test]
async fn a_redirect_to_another_host_is_not_followed() {
    let elsewhere = web(Arc::new(|_| Resp::json(json!({ "ok": true })))).await;
    let target = format!("http://{}/api/chat.postMessage", elsewhere.addr);
    let w = web(Arc::new(move |_| Resp {
        status: 302,
        headers: vec![("Location".into(), target.clone())],
        body: String::new(),
    }))
    .await;
    let t = LiveTransport::for_test(&w.base());
    let err = t
        .call("chat.postMessage", json!({}), BOT)
        .await
        .unwrap_err();
    assert!(matches!(err, TransportError::Unavailable(_)), "{err:?}");
    assert!(elsewhere.reqs().is_empty(), "the redirect was followed");
    assert!(!format!("{err}").contains(BOT));
}

#[tokio::test]
async fn an_error_that_echoes_the_token_is_scrubbed() {
    let w = web(Arc::new(|_| {
        Resp::json(json!({ "ok": false, "error": format!("bad_{BOT}") }))
    }))
    .await;
    let t = LiveTransport::for_test(&w.base());
    let err = t
        .call("chat.postMessage", json!({}), BOT)
        .await
        .unwrap_err();
    assert!(!format!("{err} {err:?}").contains(BOT), "{err:?}");
}

// ---- Socket Mode through the hub --------------------------------------------------------------

struct Rig {
    _tmp: tempfile::TempDir,
    _lock: StateLock,
    dir: PathBuf,
    web: Web,
    ws: Ws,
    calls: Arc<Mutex<Calls>>,
    hsp: HostedNotificationChannelClient<Channel>,
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

async fn rig(pin: Option<&str>) -> Rig {
    captured_logs();
    let tmp = tempfile::tempdir().unwrap();
    let lock = StateLock::open(&tmp.path().join("state")).unwrap();
    let dir = lock.dir().to_path_buf();
    let ws = ws_server().await;
    let web = web(happy(ws.addr)).await;

    let calls = Arc::new(Mutex::new(Calls::default()));
    let gw_sock = tmp.path().join("gw.sock");
    let gw_listener = bind_unix_listener(&gw_sock).unwrap();
    tokio::spawn(
        Server::builder()
            .add_service(HostedNotificationGatewayServer::new(MockGateway {
                calls: calls.clone(),
                pin: pin.map(String::from),
            }))
            .serve_with_incoming(UnixListenerStream::new(gw_listener)),
    );

    let registry = Registry::default().with(Arc::new(adapter(&web.base())));
    let hub = Hub::new(
        registry,
        DeliveryLog::open(&lock.db_path()).unwrap(),
        GatewayClient::new(Some(gw_sock)),
    );
    tokio::spawn(hub.clone().run_connection("slack"));
    let sock = tmp.path().join("n.sock");
    let listener = bind_unix_listener(&sock).unwrap();
    tokio::spawn(serve_unix(listener, NotifyBroker::new(hub)));
    let hsp = HostedNotificationChannelClient::new(dial(sock).await);
    Rig {
        _tmp: tmp,
        _lock: lock,
        dir,
        web,
        ws,
        calls,
        hsp,
    }
}

fn delivery(id: &str) -> DeliverNotificationRequest {
    DeliverNotificationRequest {
        delivery_id: id.into(),
        channel: "slack".into(),
        channel_address: "T1/U1".into(),
        notification_id: "nt_1".into(),
        kind: "approval.pending".into(),
        rendering: Some(rendering()),
        action_handle: HANDLE.into(),
        update_of: String::new(),
        resolved_secrets: [("bot_token".to_string(), BOT.to_string())].into(),
        trace_context: None,
    }
}

fn press(envelope: &str, team: &str, user: &str, delivery: &str, action: &str) -> Value {
    json!({
        "type": "interactive",
        "envelope_id": envelope,
        "accepts_response_payload": false,
        "payload": {
            "type": "block_actions",
            "team": { "id": team },
            "user": { "id": user },
            "actions": [{
                "action_id": format!("vam_act:{action}"),
                "block_id": "vam_actions",
                "value": format!("{delivery}|{action}")
            }]
        }
    })
}

async fn deliver_one(r: &mut Rig, id: &str) {
    let out = r
        .hsp
        .deliver_notification(delivery(id))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(out.outcome, DeliverOutcome::Sent as i32, "{out:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_then_a_button_press_is_acked_and_relayed_to_the_gateway() {
    let mut r = rig(None).await;
    eventually("the socket opens", || {
        r.ws.conns.load(Ordering::SeqCst) == 1
    })
    .await;
    deliver_one(&mut r, "nd_1").await;

    r.ws.send(press("env-1", "T1", "U1", "nd_1", "approve"));
    eventually("the click reaches ActOnNotification", || {
        r.calls.lock().unwrap().acts.len() == 1
    })
    .await;

    let calls = r.calls.lock().unwrap();
    let act = &calls.acts[0];
    assert_eq!(act.decision, "APPROVED");
    assert_eq!(act.handle, HANDLE);
    let who = act.external_account.as_ref().unwrap();
    assert_eq!((who.scope.as_str(), who.id.as_str()), ("T1", "U1"));
    drop(calls);

    let acks = r.ws.acks.lock().unwrap().clone();
    assert_eq!(acks, vec![json!({ "envelope_id": "env-1" })]);

    // The connect used the app token from GetChannelConfig; the open call carried it, once.
    let opens = r.web.of("apps.connections.open");
    assert_eq!(opens.len(), 1);
    assert_eq!(opens[0].auth, format!("Bearer {APP_PREFIX}1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dm_with_a_code_is_acked_and_linked() {
    let r = rig(None).await;
    eventually("the socket opens", || {
        r.ws.conns.load(Ordering::SeqCst) == 1
    })
    .await;
    r.ws.send(json!({
        "type": "events_api", "envelope_id": "env-dm",
        "payload": { "team_id": "T1", "event": {
            "type": "message", "channel_type": "im", "user": "U1", "text": "ABCDE-FGHJK" } }
    }));
    eventually("the link reaches the gateway", || {
        r.calls.lock().unwrap().links.len() == 1
    })
    .await;
    let calls = r.calls.lock().unwrap();
    assert_eq!(calls.links[0].code, "ABCDE-FGHJK");
    drop(calls);
    assert_eq!(
        r.ws.acks.lock().unwrap().clone(),
        vec![json!({ "envelope_id": "env-dm" })]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disconnect_reconnects_and_fetches_the_app_token_again() {
    let mut r = rig(None).await;
    eventually("the first socket", || {
        r.ws.conns.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(r.calls.lock().unwrap().connect_configs, 1);
    deliver_one(&mut r, "nd_1").await;

    r.ws.send(json!({ "type": "disconnect", "reason": "refresh_requested" }));
    eventually("the second socket", || {
        r.ws.conns.load(Ordering::SeqCst) == 2
    })
    .await;
    assert_eq!(r.calls.lock().unwrap().connect_configs, 2);
    let opens = r.web.of("apps.connections.open");
    assert_eq!(opens.len(), 2);
    assert_eq!(opens[0].auth, format!("Bearer {APP_PREFIX}1"));
    assert_eq!(opens[1].auth, format!("Bearer {APP_PREFIX}2"));

    // The new connection works.
    r.ws.send(press("env-2", "T1", "U1", "nd_1", "deny"));
    eventually("a click on the new socket", || {
        r.calls.lock().unwrap().acts.len() == 1
    })
    .await;
    assert_eq!(r.calls.lock().unwrap().acts[0].decision, "REJECTED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_from_another_workspace_are_ignored_when_one_is_pinned() {
    let mut r = rig(Some("T1")).await;
    eventually("the socket opens", || {
        r.ws.conns.load(Ordering::SeqCst) == 1
    })
    .await;
    deliver_one(&mut r, "nd_1").await;
    r.ws.send(press("env-x", "T9", "U9", "nd_1", "approve"));
    r.ws.send(press("env-y", "T1", "U1", "nd_1", "deny"));
    eventually("the pinned team's click", || {
        !r.calls.lock().unwrap().acts.is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let calls = r.calls.lock().unwrap();
    assert_eq!(calls.acts.len(), 1, "the other team's press was relayed");
    assert_eq!(calls.acts[0].decision, "REJECTED");
}

#[test]
fn the_event_type_is_what_the_hub_reads() {
    // A guard that `scope()` follows both variants.
    let c = InboundEvent::Click {
        delivery_id: "d".into(),
        scope: "T1".into(),
        user: "U".into(),
        decision: "x".into(),
        response: None,
    };
    assert_eq!(c.scope(), "T1");
}

// ---- tokens -----------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_token_or_ticket_reaches_logs_errors_or_the_state_dir() {
    let mut r = rig(None).await;
    eventually("the socket opens", || {
        r.ws.conns.load(Ordering::SeqCst) == 1
    })
    .await;
    deliver_one(&mut r, "nd_1").await;
    r.ws.send(press("env-1", "T1", "U1", "nd_1", "approve"));
    r.ws.send(json!({ "type": "disconnect", "reason": "refresh_requested" }));
    eventually("a second socket", || r.ws.conns.load(Ordering::SeqCst) == 2).await;
    eventually("the click", || !r.calls.lock().unwrap().acts.is_empty()).await;

    // Errors: a refused call, and a socket URL that does not answer (its ticket must not show).
    let bad = web(Arc::new(|_| {
        Resp::json(json!({ "ok": false, "error": "invalid_auth" }))
    }))
    .await;
    let t = LiveTransport::for_test(&bad.base());
    let mut shown = String::new();
    let e = t
        .call("chat.postMessage", json!({}), BOT)
        .await
        .unwrap_err();
    shown.push_str(&format!("{e} {e:?}"));
    let e = t.open_socket(&format!("{APP_PREFIX}x")).await.unwrap_err();
    shown.push_str(&format!("{e} {e:?}"));
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);
    let w = web(happy(dead_addr)).await;
    let t = LiveTransport::for_test(&w.base());
    let e = t.open_socket(&format!("{APP_PREFIX}x")).await.unwrap_err();
    shown.push_str(&format!("{e} {e:?} {t:?}"));
    // A non-wss URL from the API is refused, without echoing it.
    let plain = web(Arc::new(|_| {
        Resp::json(json!({ "ok": true, "url": format!("https://evil.example/?ticket={TICKET}") }))
    }))
    .await;
    let e = LiveTransport::for_test(&plain.base())
        .open_socket(&format!("{APP_PREFIX}x"))
        .await;
    // for_test allows ws://, but https:// is still not a socket scheme.
    let e = e.unwrap_err();
    shown.push_str(&format!("{e} {e:?}"));

    for secret in [BOT, TICKET, APP_PREFIX, HANDLE] {
        assert!(!shown.contains(secret), "an error shows {secret}");
        assert!(!logs_text().contains(secret), "a log line shows {secret}");
    }
    assert!(!logs_text().is_empty(), "the capture saw nothing");
    let hits = files_holding(&r.dir, &[BOT, TICKET, APP_PREFIX, HANDLE]);
    assert!(hits.is_empty(), "{hits:?}");
}
