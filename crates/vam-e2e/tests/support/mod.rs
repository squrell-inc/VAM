//! A stateful fake vamd for the engine's self-tests: an axum server on a thread whose state
//! survives a restart, and a scripted run that advances by one state per status poll.
#![allow(dead_code)]

use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::Router;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vam_e2e::vamd::Vamd;

#[derive(Clone, Default)]
pub struct Plan {
    /// Run states served one per status poll; the last one sticks.
    pub states: Vec<&'static str>,
    /// States after an approval or gate decision.
    pub after_decide: Vec<&'static str>,
    pub approval: Option<(&'static str, &'static str)>, // (kind, tool)
    /// Events, served once `events_after_polls` event polls have happened.
    pub events: Vec<(&'static str, Value)>,
    pub events_after_polls: usize,
    pub steps: Vec<Value>,
    /// Extra claims merged into the pending approval (`alwaysOffered`, `requestedArgs`, ...).
    pub claims: Value,
    /// The decide answers this 4xx and the approval stays pending.
    pub refuse_decide: Option<u16>,
}

pub struct FakeRun {
    pub id: String,
    pub session: String,
    pub ws: String,
    pub thread: String,
    pub current: String,
    pub queue: VecDeque<&'static str>,
    pub plan: Plan,
    pub event_polls: usize,
    pub decided: bool,
    /// The bearer that started the run: approvals are listed to it only.
    pub owner: String,
}

#[derive(Default)]
pub struct Fake {
    pub requests: Mutex<Vec<(String, String, String)>>,
    pub runs: Mutex<Vec<FakeRun>>,
    pub plans: Mutex<VecDeque<Plan>>,
    pub guest_mode: Mutex<Option<String>>,
    /// Binding ids `GET /v1/secrets` lists.
    pub secrets: Mutex<Vec<String>>,
    /// Merged into `status` of `GET /v1/node`.
    pub node_extra: Mutex<Option<Value>>,
    pub files: Mutex<HashMap<String, String>>,
    pub refuse_runs: Mutex<Option<String>>,
    /// `(method, path, bearer)` of every request.
    pub auths: Mutex<Vec<(String, String, String)>>,
    /// Answers 429 to one key's requests of one path (see `Throttle`).
    pub throttle: Mutex<Option<Throttle>>,
    /// Raw bodies POSTed to `/v1/skills/packages`.
    pub packages: Mutex<Vec<Vec<u8>>>,
    /// Pages served by `GET /v1/approvals?pageToken=pN` (first page without a token).
    pub approval_pages: Mutex<Vec<Value>>,
}

/// A fake rate limiter: requests of `path` whose bearer starts with `bearer_prefix` are answered
/// 200 `allow` times, then 429 with `Retry-After: 1`, until `window` after the first 429.
pub struct Throttle {
    pub path: &'static str,
    pub bearer_prefix: &'static str,
    pub allow: u32,
    pub window: Duration,
    pub served: u32,
    pub first_limited: Option<std::time::Instant>,
    /// Send `Retry-After` on the 429 (false: a refusal without it).
    pub retry_after: bool,
}

impl Fake {
    pub fn new() -> Arc<Fake> {
        Arc::new(Fake::default())
    }
    pub fn plan(&self, p: Plan) {
        self.plans.lock().unwrap().push_back(p);
    }
    pub fn requests_to(&self, method: &str, prefix: &str) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, p, _)| m == method && p.starts_with(prefix))
            .map(|(_, _, b)| serde_json::from_str(b).unwrap_or(Value::Null))
            .collect()
    }
    /// The bearers used on requests of `method` whose path starts with `prefix`.
    pub fn bearers(&self, method: &str, prefix: &str) -> Vec<String> {
        self.auths
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, p, _)| m == method && p.starts_with(prefix))
            .map(|(_, _, b)| b.clone())
            .collect()
    }
    pub fn count(&self, method: &str, prefix: &str) -> usize {
        self.requests_to(method, prefix).len()
    }
}

fn unseg(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The routes the new runner forms need (throttle, secrets, skill packages, receipt, approval
/// pages); everything else goes to `handle_core`.
async fn handle(
    State(f): State<Arc<Fake>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = uri.path().to_string();
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_start_matches("Bearer ").to_string())
        .unwrap_or_default();
    let record = |f: &Fake, text: String| {
        f.requests
            .lock()
            .unwrap()
            .push((method.to_string(), uri.to_string(), text));
        f.auths
            .lock()
            .unwrap()
            .push((method.to_string(), uri.to_string(), bearer.clone()));
    };
    if let Some(t) = f.throttle.lock().unwrap().as_mut() {
        if path == t.path && bearer.starts_with(t.bearer_prefix) {
            if t.first_limited.is_some_and(|at| at.elapsed() > t.window) {
                t.first_limited = None;
                t.served = 0;
            }
            t.served += 1;
            if t.served > t.allow {
                record(&f, String::new());
                t.first_limited.get_or_insert_with(std::time::Instant::now);
                let mut h = axum::http::HeaderMap::new();
                if t.retry_after {
                    h.insert("retry-after", "1".parse().unwrap());
                }
                h.insert("x-vam-rate-limited-reason", "key-rate".parse().unwrap());
                return (StatusCode::TOO_MANY_REQUESTS, h, "{}".to_string()).into_response();
            }
        }
    }
    if path == "/v1/secrets" && method == Method::PUT {
        record(&f, String::from_utf8_lossy(&body).to_string());
        return (StatusCode::OK, "{}".to_string()).into_response();
    }
    if path == "/v1/skills/packages" && method == Method::POST {
        record(&f, format!("<{} bytes>", body.len()));
        f.packages.lock().unwrap().push(body.to_vec());
        return (StatusCode::OK, "{}".to_string()).into_response();
    }
    if path.starts_with("/v1/runs/") && path.ends_with("/receipt") {
        record(&f, String::new());
        let canned = f.files.lock().unwrap().get("receipt").cloned();
        return match canned {
            Some(c) => (StatusCode::OK, fill(&f, c)).into_response(),
            None => (StatusCode::NOT_FOUND, "{}".to_string()).into_response(),
        };
    }
    // Agent reads: the draft (`files["agent-draft"]`, else a small spec), and the outcomes routes
    // (`files["outcomes"]`, `files["outcomes-diff"]`; 404 until a test sets them).
    if method == Method::GET && path.starts_with("/v1/agents/") {
        let diff = path.ends_with("/outcomes/diff");
        if diff || path.ends_with("/outcomes") {
            record(&f, uri.to_string());
            let canned = f
                .files
                .lock()
                .unwrap()
                .get(if diff { "outcomes-diff" } else { "outcomes" })
                .cloned();
            return match canned {
                Some(c) => (StatusCode::OK, c).into_response(),
                None => (StatusCode::NOT_FOUND, "{}".to_string()).into_response(),
            };
        }
        if !path.ends_with("/versions") && !path.contains("/permissions") {
            record(&f, String::new());
            let draft = f.files.lock().unwrap().get("agent-draft").cloned();
            let body = draft.unwrap_or_else(|| {
                json!({"spec": {"resourceId": {"urn": unseg(&path["/v1/agents/".len()..])},
                    "description": "first edition", "permissions": {"tools": {"default": "deny"}}}})
                .to_string()
            });
            return (StatusCode::OK, body).into_response();
        }
    }
    if path == "/v1/approvals" && !f.approval_pages.lock().unwrap().is_empty() {
        record(&f, String::new());
        let pages = f.approval_pages.lock().unwrap().clone();
        let n: usize = uri
            .query()
            .and_then(|q| q.strip_prefix("pageToken=p"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut page = pages.get(n).cloned().unwrap_or(json!({"approvals": []}));
        if n + 1 < pages.len() {
            page["pagination"] = json!({"nextPageToken": format!("p{}", n + 1)});
        }
        return (StatusCode::OK, page.to_string()).into_response();
    }
    let (code, text) = handle_core(
        State(f.clone()),
        method,
        uri,
        headers,
        String::from_utf8_lossy(&body).to_string(),
    )
    .await;
    (code, fill(&f, text)).into_response()
}

/// Fills `{{SECRET}}`, `{{SECRET_BASE64}}`, `{{SECRET_HEX}}`, `{{SECRET_JSON}}` and the two
/// halves with the last secret the engine PUT, so a canned body can leak it in a chosen form.
fn fill(f: &Fake, text: String) -> String {
    if !text.contains("{{SECRET") {
        return text;
    }
    let value = f
        .requests_to("PUT", "/v1/secrets")
        .last()
        .and_then(|b| b["value"].as_str().map(String::from))
        .unwrap_or_default();
    use base64::Engine as _;
    let json = serde_json::to_string(&value).unwrap_or_default();
    let (a, b) = value.split_at(value.len() / 2);
    let esc = |t: &str| {
        serde_json::to_string(t)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string()
    };
    let (a, b) = (a, b);
    text.replace(
        "{{SECRET_BASE64}}",
        &base64::engine::general_purpose::STANDARD.encode(&value),
    )
    .replace(
        "{{SECRET_HEX}}",
        &value
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    )
    .replace("{{SECRET_JSON}}", json.trim_matches('"'))
    // The halves go into JSON bodies: escaped, so a quote in the value keeps the body valid.
    .replace("{{SECRET_FIRST_HALF}}", &esc(a))
    .replace("{{SECRET_SECOND_HALF}}", &esc(b))
    .replace("{{SECRET}}", &value)
}

async fn handle_core(
    State(f): State<Arc<Fake>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, String) {
    let path = uri.path().to_string();
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_start_matches("Bearer ").to_string())
        .unwrap_or_default();
    f.auths
        .lock()
        .unwrap()
        .push((method.to_string(), uri.to_string(), bearer.clone()));
    f.requests
        .lock()
        .unwrap()
        .push((method.to_string(), uri.to_string(), body.clone()));
    let ok = |v: Value| (StatusCode::OK, v.to_string());
    let m = method.as_str();
    if path == "/v1/health" {
        return ok(json!({"status": "SERVING_STATUS_SERVING"}));
    }
    if path == "/v1/node" {
        let mode = f.guest_mode.lock().unwrap().clone();
        let mut st = json!({"nodeId": "local"});
        if let Some(m) = mode {
            st["guestMode"] = json!(m);
        }
        if let Some(Value::Object(x)) = f.node_extra.lock().unwrap().clone() {
            for (k, v) in x {
                st[k] = v;
            }
        }
        return ok(json!({"status": st}));
    }
    if path == "/v1/secrets" && m == "GET" {
        let ids = f.secrets.lock().unwrap().clone();
        let list: Vec<Value> = ids.iter().map(|i| json!({"bindingId": i})).collect();
        return ok(json!({"secrets": list}));
    }
    if path == "/v1/node/allowed-models" {
        return ok(json!({"models": ["mock-model"]}));
    }
    if (path == "/v1/agents" || path == "/v1/playbooks") && m == "PUT" {
        return ok(json!({}));
    }
    if path == "/v1/users" && m == "POST" {
        let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        return ok(json!({"user": {"userId": req["username"], "role": req["role"]}}));
    }
    if path.starts_with("/v1/users/") && path.ends_with("/keys") && m == "POST" {
        let id = unseg(
            path.trim_start_matches("/v1/users/")
                .trim_end_matches("/keys"),
        );
        return ok(json!({"key": {"keyId": "k1"}, "secret": format!("secret-of-{id}")}));
    }
    if path.ends_with("/release") {
        return ok(json!({}));
    }
    if path == "/v1/runs" && m == "POST" {
        if let Some(msg) = f.refuse_runs.lock().unwrap().clone() {
            return (StatusCode::BAD_REQUEST, msg);
        }
        let plan = f.plans.lock().unwrap().pop_front().unwrap_or(Plan {
            states: vec!["RUN_STATE_RUNNING", "RUN_STATE_SUCCEEDED"],
            ..Default::default()
        });
        let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let mut runs = f.runs.lock().unwrap();
        let n = runs.len() + 1;
        let id = format!("urn:vam:run:local.vam.dev/acme/run-{n}");
        let thread = req["spec"]["threadRef"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        // The node reports ids; a request that names them (a follow-up) keeps them.
        let space_id = req["spec"]["spaceId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("sp_{n}"));
        let thread_id = req["spec"]["threadId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("th_{n}"));
        let ws = space_id.clone();
        runs.push(FakeRun {
            id: id.clone(),
            session: format!("sess-{n}"),
            ws: ws.clone(),
            thread: thread.clone(),
            current: "RUN_STATE_PENDING".into(),
            queue: plan.states.iter().copied().collect(),
            plan,
            event_polls: 0,
            decided: false,
            owner: bearer.clone(),
        });
        return ok(
            json!({"status": {"runId": {"urn": id}, "state": "RUN_STATE_PENDING"},
            "workspaceId": ws, "threadRef": thread, "spaceId": space_id,
            "threadId": thread_id, "sessionId": format!("sess-{n}")}),
        );
    }
    if let Some(rest) = path.strip_prefix("/v1/runs/") {
        let (id, tail) = match rest.rsplit_once('/') {
            Some((a, b)) if b == "events" || b == "cancel" || b == "stop" => {
                (unseg(a), b.to_string())
            }
            _ => (unseg(rest), String::new()),
        };
        let mut runs = f.runs.lock().unwrap();
        let Some(run) = runs.iter_mut().find(|r| r.id == id) else {
            return (StatusCode::NOT_FOUND, "{}".into());
        };
        match tail.as_str() {
            "stop" => {
                run.queue = VecDeque::from(vec!["RUN_STATE_FAILED"]);
                return ok(json!({}));
            }
            "cancel" => {
                run.current = "RUN_STATE_CANCELLED".into();
                run.queue.clear();
                return ok(json!({}));
            }
            "events" => {
                run.event_polls += 1;
                let evs: Vec<Value> = if run.event_polls > run.plan.events_after_polls {
                    run.plan
                        .events
                        .iter()
                        .enumerate()
                        .map(|(i, (t, p))| json!({"sequence": i + 1, "eventType": t, "payload": p.to_string()}))
                        .collect()
                } else {
                    vec![]
                };
                let after: usize = uri
                    .query()
                    .and_then(|q| q.strip_prefix("after="))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let evs: Vec<Value> = evs
                    .into_iter()
                    .filter(|e| e["sequence"].as_u64().unwrap_or(0) as usize > after)
                    .collect();
                return ok(json!({"events": evs}));
            }
            _ => {
                if let Some(next) = run.queue.pop_front() {
                    run.current = next.into();
                }
                return ok(
                    json!({"status": {"runId": {"urn": run.id}, "state": run.current,
                    "stepStatuses": run.plan.steps, "sessionId": run.session}}),
                );
            }
        }
    }
    if path == "/v1/approvals" {
        let runs = f.runs.lock().unwrap();
        let list: Vec<Value> = runs
            .iter()
            .filter(|r| r.current == "RUN_STATE_PAUSED" && !r.decided && r.owner == bearer)
            .filter_map(|r| {
                r.plan.approval.map(|(kind, tool)| {
                    let mut claims = json!({"requestId": {"urn": format!("req-{}", r.id)}, "runId": {"urn": r.id},
                        "stepId": "confirm", "kind": kind, "targetToolName": tool});
                    if let (Some(extra), Some(c)) = (r.plan.claims.as_object(), claims.as_object_mut()) {
                        for (k, v) in extra {
                            c.insert(k.clone(), v.clone());
                        }
                    }
                    json!({ "claims": claims })
                })
            })
            .collect();
        return ok(json!({"approvals": list}));
    }
    if path.starts_with("/v1/approvals/") && path.ends_with("/decide") {
        let mut runs = f.runs.lock().unwrap();
        if let Some(code) = runs
            .iter()
            .find(|r| r.current == "RUN_STATE_PAUSED")
            .and_then(|r| r.plan.refuse_decide)
        {
            return (
                StatusCode::from_u16(code).unwrap(),
                r#"{"error":"refused"}"#.into(),
            );
        }
        for r in runs.iter_mut().filter(|r| r.current == "RUN_STATE_PAUSED") {
            r.decided = true;
            r.queue = r.plan.after_decide.iter().copied().collect();
        }
        return ok(json!({}));
    }
    if path.starts_with("/v1/sessions/") && path.ends_with("/terminate") {
        let sid = unseg(
            path.trim_start_matches("/v1/sessions/")
                .trim_end_matches("/terminate"),
        );
        let mut runs = f.runs.lock().unwrap();
        for r in runs.iter_mut().filter(|r| r.session == sid) {
            r.queue = VecDeque::from(vec!["RUN_STATE_CANCELLED"]);
        }
        return ok(json!({}));
    }
    if let Some(rest) = path.strip_prefix("/v1/workspaces/") {
        // `/v1/workspaces/{ws}/threads/{thread}/messages`: one canned body per workspace.
        let key = match rest.split_once("/threads/") {
            Some((ws, tail)) if tail.ends_with("/messages") => format!("{ws}/threads/messages"),
            _ => rest.replace("/files/", "/"),
        };
        return match f.files.lock().unwrap().get(&key) {
            Some(c) => (StatusCode::OK, c.clone()),
            None => (StatusCode::NOT_FOUND, "{}".into()),
        };
    }
    (StatusCode::NOT_FOUND, "{}".into())
}

struct Server {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

pub struct FakeVamd {
    pub state: Arc<Fake>,
    port: u16,
    server: Option<Server>,
    pub restart_delay: Duration,
    pub fail_restart: bool,
}

impl FakeVamd {
    pub fn new(state: Arc<Fake>) -> FakeVamd {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        FakeVamd {
            state,
            port,
            server: None,
            restart_delay: Duration::ZERO,
            fail_restart: false,
        }
    }

    fn serve(&mut self, delay: Duration) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let state = self.state.clone();
        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", self.port).parse().unwrap();
        let join = std::thread::spawn(move || {
            std::thread::sleep(delay);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let app = Router::new().fallback(handle).with_state(state);
                let mut server = None;
                for _ in 0..40 {
                    match axum::Server::try_bind(&addr) {
                        Ok(s) => {
                            server = Some(s);
                            break;
                        }
                        Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                    }
                }
                let Some(server) = server else { return };
                let _ = server
                    .serve(app.into_make_service())
                    .with_graceful_shutdown(async {
                        let _ = rx.await;
                    })
                    .await;
            });
        });
        self.server = Some(Server {
            stop: Some(tx),
            join: Some(join),
        });
    }

    /// Simulates vamd going away for good.
    pub fn kill(&mut self) {
        self.stop();
    }
}

impl Vamd for FakeVamd {
    fn start(&mut self) -> Result<(), String> {
        self.serve(Duration::ZERO);
        Ok(())
    }
    fn stop(&mut self) {
        if let Some(mut s) = self.server.take() {
            if let Some(tx) = s.stop.take() {
                let _ = tx.send(());
            }
            // Idle keep-alive connections end the graceful shutdown; do not wait forever.
            if let Some(j) = s.join.take() {
                let start = std::time::Instant::now();
                while !j.is_finished() && start.elapsed() < Duration::from_secs(2) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
    fn restart(&mut self) -> Result<(), String> {
        if self.fail_restart {
            self.stop();
            return Err("fake vamd does not come back".into());
        }
        self.stop();
        self.serve(self.restart_delay);
        Ok(())
    }
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    fn alive(&mut self) -> bool {
        self.server.is_some()
    }
    fn log_tail(&self, _lines: usize) -> String {
        "(fake vamd log)".into()
    }
    fn guest_logs(&self) -> Vec<PathBuf> {
        vec![]
    }
}
