//! The steps for connectors and the two runner-hosted mock providers.
//!
//! Steps: `connector-node`, `connector-connect`, `connector-check`, `mock-provider`; expects:
//! `connectors` and `mock-requests`. The wire shapes are written down in `e2e/README.md` under
//! "Connectors".
//!
//! The providers are std-only loopback HTTP servers, started once per run and reset by each
//! `mock-provider` piece:
//! - `microsoft-login` serves `POST /{tenant}/oauth2/v2.0/token` for `refresh_token` grants. It
//!   accepts only refresh tokens the runner registered, logs form field NAMES (and the values of
//!   a few non-secret fields), and can be revoked.
//! - `linear-server` serves a minimal streamable-HTTP MCP server at `/mcp` and requires the
//!   runner-made bearer.
//!
//! Token values are made here, kept in memory, added to the engine's scrub list and never printed
//! or logged. This module is a child of `engine`, so it reads the engine's private state.

use super::{conn, Engine, Fail, Secret};
use crate::api::seg;
use crate::scenario::Class;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// The client id the runner gives `vamd` for `microsoft-365` (not a secret, not a real app).
pub const TEST_CLIENT_ID: &str = "e2e-microsoft-365-client";
/// The environment variable that names it.
pub const CLIENT_ID_ENV: &str = "VAMD_CONNECTOR_MICROSOFT_365_CLIENT_ID";
/// The process-only seam that maps the providers' hosts to the mocks.
pub const ENDPOINTS_ENV: &str = "VAMD_PROCESS_CONNECTOR_ENDPOINTS";
/// `vamd` refuses an unknown `VAMD_PROCESS_*` variable until it knows this one. The wave that
/// adds the setting to vamd's known list flips this to `true`.
pub const PASS_CONNECTOR_ENDPOINTS: bool = true;
/// Add the `microsoft-graph` key to the seam: the egress gate answers `graph.microsoft.com` from
/// the Graph mock. On since CN-2 W2 (a default vamd refuses the whole variable).
pub const PASS_GRAPH_ENDPOINT: bool = true;

/// Add the `mcp:<url>` keys to the endpoint seam: the MCP server of a connector a marketplace
/// installed (`COMMUNITY_MCP_URL`) is answered by the Linear mock. Off until the node knows the keys
/// (a default vamd refuses the whole variable); on since CN-4.11.
pub const PASS_MCP_SERVER_SEAM: bool = true;
/// The MCP URL of the `bugwatch` connector in the `marketplace-community-connectors` fixture.
pub const COMMUNITY_MCP_URL: &str = "https://mcp.bugwatch.example/mcp";

/// Give `vamd` the marketplace seam (`VAMD_PROCESS_MARKETPLACE_URLS`): a JSON map from the https URL
/// of a marketplace to the `file://` bare repo the runner made. On since CN-4.4 (a default vamd
/// refuses the variable, and has no `file://` transport at all).
pub const PASS_MARKETPLACE_SEAM: bool = true;
/// The environment variable that names the seam.
pub const MARKETPLACE_SEAM_ENV: &str = "VAMD_PROCESS_MARKETPLACE_URLS";

/// Linear's MCP URL as `spec/connectors/linear.json` fixes it.
const LINEAR_MCP_URL: &str = "https://mcp.linear.app/mcp";

/// The environment `vamd` is given when mock providers run: the test client id always, and the
/// endpoint seam once `PASS_CONNECTOR_ENDPOINTS` is on.
pub fn node_env(m: &Mocks) -> Vec<(String, String)> {
    let mut env = vec![(CLIENT_ID_ENV.to_string(), TEST_CLIENT_ID.to_string())];
    // The wave that adds the setting to vamd flips PASS_CONNECTOR_ENDPOINTS.
    if PASS_CONNECTOR_ENDPOINTS {
        env.push((ENDPOINTS_ENV.to_string(), m.endpoints_json()));
    }
    env
}

// ---- step and expect shapes ------------------------------------------------------------------

/// `connector-node: { id, enabled, consent: { tenant }, as, refused }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorNode {
    pub id: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub consent: Option<Consent>,
    /// Alias of the `org` piece that acts; the bootstrap key when omitted.
    #[serde(rename = "as")]
    pub as_: Option<String>,
    /// The step passes only when vamd answers 400, 403 or 409 (a 404 or 405 is a missing route).
    #[serde(default)]
    pub refused: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Consent {
    pub tenant: String,
}

fn yes() -> bool {
    true
}

/// `connector-connect: { id, url, piece, tenant, upn, as, refused }`. `microsoft-365` connects
/// through `PUT /v1/connectors/{id}/connection`; any other id through the MCP routes.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectorConnect {
    pub id: String,
    /// MCP only: the server URL. With `piece`, the provider's address when omitted.
    pub url: Option<String>,
    /// Alias of a `mock-provider` piece: its address and token are used.
    pub piece: Option<String>,
    /// `microsoft-365` only: the `tid` claim of the id token.
    pub tenant: Option<String>,
    /// `microsoft-365` only: the `preferred_username` claim.
    pub upn: Option<String>,
    /// `microsoft-365` only: lifetime in seconds of the access token handed over at connect.
    /// `0` makes the token stale at once (the mock then counts it as `bearer=stale`).
    pub expires_in: Option<u64>,
    #[serde(rename = "as")]
    pub as_: Option<String>,
    #[serde(default)]
    pub refused: bool,
}

/// `connector-check: { id, as }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorCheck {
    pub id: String,
    #[serde(rename = "as")]
    pub as_: Option<String>,
}

/// `mock-provider: { piece, revoke }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockProviderStep {
    pub piece: String,
    /// The provider refuses everything from now on (`invalid_grant` / 401).
    #[serde(default)]
    pub revoke: bool,
    /// Graph only: `true` makes `POST /v1.0/me/sendMail` log the request and then wait without
    /// answering; `false` releases it (and later requests answer at once).
    pub hold: Option<bool>,
}

/// `connectors: { id, as, fields, absent }`: one connector of `GET /v1/connectors` for a caller.
/// `fields` are dotted paths with exact values; `absent` are paths that must not exist.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorsExpect {
    pub id: String,
    #[serde(rename = "as")]
    pub as_: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
    #[serde(default)]
    pub absent: Vec<String>,
}

/// `mock-requests: { piece, path, has, lacks, count }`: what a provider saw at `path`. `has` and
/// `lacks` name fields (`client_id`, `client_id=<value>`, `tool=list_issues`, `authorization`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockRequestsExpect {
    pub piece: String,
    /// The request path; `*` matches every path.
    pub path: String,
    /// Only requests with this HTTP method (`GET`, `POST`).
    pub method: Option<String>,
    #[serde(default)]
    pub has: Vec<String>,
    #[serde(default)]
    pub lacks: Vec<String>,
    pub count: Option<usize>,
}

// ---- mock providers --------------------------------------------------------------------------

/// One request, as logged: no secret value.
#[derive(Debug, Clone)]
struct Logged {
    path: String,
    method: String,
    /// Field names, and `name=value` for the few non-secret ones.
    fields: BTreeSet<String>,
}

#[derive(Default)]
struct State {
    log: Vec<Logged>,
    revoked: bool,
    /// Login: the access token a connect or the latest refresh made valid.
    current: String,
    /// Login: access tokens that were valid once and are not now.
    stale: BTreeSet<String>,
    /// Linear: the bearer the server requires.
    bearer: String,
    /// Login: the refresh tokens the runner registered.
    refresh: BTreeSet<String>,
    /// Graph: `sendMail` is logged and then held until released.
    hold: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Login,
    Linear,
    Graph,
}

/// The three providers.
pub struct Mocks {
    login: Arc<Mutex<State>>,
    linear: Arc<Mutex<State>>,
    graph: Arc<Mutex<State>>,
    login_addr: SocketAddr,
    linear_addr: SocketAddr,
    graph_addr: SocketAddr,
}

struct Req {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: String,
}

impl Mocks {
    /// Binds both providers on loopback and serves them on threads until the process ends.
    pub fn start() -> std::io::Result<Arc<Mocks>> {
        let login = Arc::new(Mutex::new(State::default()));
        let linear = Arc::new(Mutex::new(State::default()));
        let graph = Arc::new(Mutex::new(State::default()));
        let l1 = TcpListener::bind("127.0.0.1:0")?;
        let l2 = TcpListener::bind("127.0.0.1:0")?;
        let l3 = TcpListener::bind("127.0.0.1:0")?;
        let m = Mocks {
            login_addr: l1.local_addr()?,
            linear_addr: l2.local_addr()?,
            graph_addr: l3.local_addr()?,
            login: login.clone(),
            linear: linear.clone(),
            graph: graph.clone(),
        };
        serve(l1, login.clone(), Kind::Login);
        serve(l2, linear, Kind::Linear);
        serve_graph(l3, graph, login);
        Ok(Arc::new(m))
    }

    /// `login` and `linear` base URLs, for the seam that maps the real hosts to the mocks.
    pub fn endpoints_json(&self) -> String {
        let mut m = json!({
            "microsoft-login": format!("http://{}", self.login_addr),
            "linear-server": format!("http://{}/mcp", self.linear_addr),
        });
        if PASS_GRAPH_ENDPOINT {
            m["microsoft-graph"] = json!(format!("http://{}", self.graph_addr));
        }
        if PASS_MCP_SERVER_SEAM {
            m[format!("mcp:{COMMUNITY_MCP_URL}")] =
                json!(format!("http://{}/mcp", self.linear_addr));
        }
        m.to_string()
    }

    fn state(&self, k: Kind) -> &Arc<Mutex<State>> {
        match k {
            Kind::Login => &self.login,
            Kind::Linear => &self.linear,
            Kind::Graph => &self.graph,
        }
    }

    fn addr(&self, k: Kind) -> SocketAddr {
        match k {
            Kind::Login => self.login_addr,
            Kind::Linear => self.linear_addr,
            Kind::Graph => self.graph_addr,
        }
    }
}

fn serve(listener: TcpListener, state: Arc<Mutex<State>>, kind: Kind) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let state = state.clone();
            std::thread::spawn(move || {
                let _ = handle(stream, &state, kind);
            });
        }
    });
}

fn serve_graph(listener: TcpListener, state: Arc<Mutex<State>>, login: Arc<Mutex<State>>) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (state, login) = (state.clone(), login.clone());
            std::thread::spawn(move || {
                if let Ok(req) = read_req(&stream) {
                    if let Ok(out) = stream.try_clone() {
                        graph(out, req, &state, &login);
                    }
                }
            });
        }
    });
}

/// The Graph mock: messages (list, `$search`, by id), calendarView, findMeetingTimes, draft, send,
/// events and sendMail (which can be held), 404 for the rest. It logs the method and a
/// `bearer=` tag (`current`, `stale`, `placeholder`, `unknown` or `none`), never a token value.
fn graph(out: TcpStream, req: Req, state: &Arc<Mutex<State>>, login: &Arc<Mutex<State>>) {
    let path = req.path.split('?').next().unwrap_or_default().to_string();
    let sent = req
        .headers
        .get("authorization")
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).to_string());
    let tag = {
        let l = login.lock().unwrap();
        match sent.as_deref() {
            None => "none",
            Some(t) if t.starts_with("vamsec_") => "placeholder",
            Some(t) if !l.current.is_empty() && t == l.current => "current",
            Some(t) if l.stale.contains(t) => "stale",
            Some(_) => "unknown",
        }
    };
    let mut fields = BTreeSet::new();
    fields.insert(format!("method={}", req.method));
    fields.insert(format!("bearer={tag}"));
    state.lock().unwrap().log.push(Logged {
        path: path.clone(),
        method: req.method.clone(),
        fields,
    });
    if tag != "current" {
        return respond(out, 401, "application/json", r#"{"error":"unauthorized"}"#);
    }
    let query = req.path.split_once('?').map(|x| x.1).unwrap_or_default();
    if req.method == "GET" && path == "/v1.0/me/messages" && query.contains("$search=") {
        if let Some(l) = state.lock().unwrap().log.last_mut() {
            l.fields.insert("search".into());
        }
    }
    let id_of = |prefix: &str| {
        path.strip_prefix(prefix)
            .filter(|r| !r.is_empty() && !r.contains('/'))
    };
    match (req.method.as_str(), path.as_str()) {
        ("GET", "/v1.0/me/messages") => {
            let body = json!({ "value": [
                { "id": "m1", "subject": "Quarterly planning notes" },
                { "id": "m2", "subject": "Lunch on Friday" }
            ]});
            respond(out, 200, "application/json", &body.to_string());
        }
        ("GET", _) if id_of("/v1.0/me/messages/").is_some() => {
            let id = id_of("/v1.0/me/messages/").unwrap_or_default();
            let body = json!({ "id": id, "subject": "Quarterly planning notes",
                "from": { "emailAddress": { "address": "ana@e2e.example" } },
                "body": { "contentType": "text", "content": "Please plan the quarter." } });
            respond(out, 200, "application/json", &body.to_string());
        }
        ("GET", "/v1.0/me/calendarView") => {
            let body = json!({ "value": [
                { "id": "e1", "subject": "Team sync",
                  "start": { "dateTime": "2026-11-10T09:00:00", "timeZone": "UTC" },
                  "end": { "dateTime": "2026-11-10T09:30:00", "timeZone": "UTC" } }
            ]});
            respond(out, 200, "application/json", &body.to_string());
        }
        ("POST", "/v1.0/me/findMeetingTimes") => {
            let body = json!({ "meetingTimeSuggestions": [
                { "meetingTimeSlot": {
                    "start": { "dateTime": "2026-11-10T10:00:00", "timeZone": "UTC" },
                    "end": { "dateTime": "2026-11-10T10:30:00", "timeZone": "UTC" } } },
                { "meetingTimeSlot": {
                    "start": { "dateTime": "2026-11-10T14:00:00", "timeZone": "UTC" },
                    "end": { "dateTime": "2026-11-10T14:30:00", "timeZone": "UTC" } } }
            ]});
            respond(out, 200, "application/json", &body.to_string());
        }
        ("POST", "/v1.0/me/messages") => respond(
            out,
            201,
            "application/json",
            &json!({ "id": "d1" }).to_string(),
        ),
        ("POST", p) if p.starts_with("/v1.0/me/messages/") && p.ends_with("/send") => {
            respond(out, 202, "application/json", "")
        }
        ("POST", "/v1.0/me/events") => respond(
            out,
            201,
            "application/json",
            &json!({ "id": "ev1" }).to_string(),
        ),
        ("POST", p) if p.starts_with("/v1.0/me/events/") && p.ends_with("/forward") => {
            respond(out, 202, "application/json", "")
        }
        ("POST", "/v1.0/me/sendMail") => {
            // Held: the request is already logged (Graph "got it"); the answer waits.
            for _ in 0..1200 {
                if !state.lock().unwrap().hold {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            respond(out, 202, "application/json", "")
        }
        _ => respond(out, 404, "text/plain", "not found"),
    }
}

fn read_req(stream: &TcpStream) -> std::io::Result<Req> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut buf = vec![0u8; len.min(1 << 20)];
    reader.read_exact(&mut buf)?;
    Ok(Req {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&buf).into_owned(),
    })
}

fn respond(mut s: TcpStream, status: u16, ctype: &str, body: &str) {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        _ => "Not Found",
    };
    let _ = write!(
        s,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn handle(stream: TcpStream, state: &Arc<Mutex<State>>, kind: Kind) -> std::io::Result<()> {
    let req = read_req(&stream)?;
    let out = stream.try_clone()?;
    match kind {
        Kind::Login => login(out, req, state),
        Kind::Linear => linear(out, req, state),
        Kind::Graph => unreachable!("served by serve_graph"),
    }
    Ok(())
}

/// Percent-decodes a form value (enough for `+` and `%XX`).
fn form_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(h, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Form fields whose value may be logged: none of them is a secret.
const LOGGED_VALUES: &[&str] = &["client_id", "grant_type", "scope"];

fn login(out: TcpStream, req: Req, state: &Arc<Mutex<State>>) {
    let token_path = req.path.ends_with("/oauth2/v2.0/token");
    if req.method != "POST" || !token_path {
        return respond(out, 404, "text/plain", "not found");
    }
    let mut fields = BTreeSet::new();
    let mut refresh = String::new();
    for pair in req.body.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let (k, v) = (form_decode(k), form_decode(v));
        if LOGGED_VALUES.contains(&k.as_str()) {
            fields.insert(format!("{k}={v}"));
        }
        if k == "refresh_token" {
            refresh = v;
        }
        fields.insert(k);
    }
    let (ok, new_access) = {
        let mut st = state.lock().unwrap();
        st.log.push(Logged {
            path: req.path.clone(),
            method: req.method.clone(),
            fields,
        });
        let ok = !st.revoked && st.refresh.contains(&refresh);
        let new_access = rand_token();
        if ok {
            let old = std::mem::replace(&mut st.current, new_access.clone());
            if !old.is_empty() {
                st.stale.insert(old);
            }
        }
        (ok, new_access)
    };
    if ok {
        let body = json!({
            "token_type": "Bearer",
            "access_token": new_access,
            "expires_in": 3600,
            "scope": "offline_access"
        });
        respond(out, 200, "application/json", &body.to_string());
    } else {
        let body = json!({ "error": "invalid_grant", "error_description": "revoked" });
        respond(out, 400, "application/json", &body.to_string());
    }
}

fn linear(out: TcpStream, req: Req, state: &Arc<Mutex<State>>) {
    if req.path != "/mcp" || req.method != "POST" {
        return respond(out, 404, "text/plain", "not found");
    }
    let rpc: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
    let method = rpc["method"].as_str().unwrap_or_default().to_string();
    let id = rpc["id"].clone();
    let sent = req
        .headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let (authed, mut fields) = {
        let st = state.lock().unwrap();
        let authed = !st.revoked && !st.bearer.is_empty() && sent.as_deref() == Some(&st.bearer);
        (authed, BTreeSet::new())
    };
    fields.insert(format!("method={method}"));
    if sent.is_some() {
        fields.insert(if authed {
            "authorization".to_string()
        } else {
            "authorization-wrong".to_string()
        });
    }
    if method == "tools/call" {
        fields.insert(format!(
            "tool={}",
            rpc["params"]["name"].as_str().unwrap_or_default()
        ));
    }
    state.lock().unwrap().log.push(Logged {
        path: req.path.clone(),
        method: req.method.clone(),
        fields,
    });
    if !authed {
        return respond(out, 401, "application/json", r#"{"error":"unauthorized"}"#);
    }
    let result = match method.as_str() {
        "initialize" => json!({
            "protocolVersion": "2025-03-26",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mock-linear", "version": "1" }
        }),
        "tools/list" => {
            let tool = |name: &str| {
                json!({ "name": name, "description": name,
                        "inputSchema": { "type": "object", "properties": {} } })
            };
            json!({ "tools": [
                tool("list_issues"), tool("get_issue"), tool("save_issue"),
                tool("save_comment"), tool("export_workspace")
            ] })
        }
        "tools/call" => json!({
            "content": [{ "type": "text", "text": "ok" }], "isError": false
        }),
        _ => return respond(out, 202, "application/json", ""),
    };
    let body = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    respond(out, 200, "application/json", &body.to_string());
}

/// `*` matches every path; a leading or trailing `*` matches a suffix or a prefix.
fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return path.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return path.starts_with(prefix);
    }
    pattern == path
}

fn rand_token() -> String {
    let mut buf = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        buf.copy_from_slice(&n.to_le_bytes());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

// ---- engine side -----------------------------------------------------------------------------

/// What the connector steps remember inside a scenario.
#[derive(Debug, Default)]
pub struct ConnState {
    /// Alias of a `mock-provider` piece -> which provider it is.
    providers: BTreeMap<String, u8>,
    /// MCP connections the steps made: `(id, key)`, removed at cleanup as the same identity.
    mcp: Vec<(String, Option<String>)>,
    /// REST connections the steps made (`microsoft-365`): `(id, key)`, disconnected at cleanup as
    /// the same identity, so a member's connection does not outlive its scenario on a shared vamd.
    rest: Vec<(String, Option<String>)>,
    /// Identities (keys) that linked a contact in this scenario; their contacts are unlinked at cleanup.
    pub contact_keys: Vec<Option<String>>,
    /// Connector ids a `connector-node` step touched; reset at cleanup as the bootstrap key.
    nodes: std::collections::BTreeSet<String>,
    /// Bearer of the Linear mock for this scenario (also in the scrub list).
    linear_bearer: String,
}

fn product(want: impl Into<String>, got: impl Into<String>) -> Fail {
    Fail::new(Class::Product, want, got)
}

fn b64url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn dotted<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |cur, k| match cur {
        Value::Array(a) => k.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => cur.get(k),
    })
}

impl Engine {
    fn mocks(&self) -> Result<Arc<Mocks>, Fail> {
        self.connector_mocks.clone().ok_or_else(|| {
            Fail::new(
                Class::Harness,
                "the runner started the mock providers",
                "none running",
            )
        })
    }

    fn remember(&mut self, alias: &str, value: &str) {
        self.ctx.secrets.insert(
            format!("connector:{alias}:{}", self.ctx.secrets.len()),
            Secret(value.to_string()),
        );
    }

    fn provider_of(&self, alias: &str) -> Result<Kind, Fail> {
        match self.ctx.conn.providers.get(alias) {
            Some(0) => Ok(Kind::Login),
            Some(2) => Ok(Kind::Graph),
            Some(_) => Ok(Kind::Linear),
            None => Err(Fail::new(
                Class::Scenario,
                format!("mock-provider alias `{alias}`"),
                "not a mock-provider piece in `given`",
            )),
        }
    }

    /// A `mock-provider` piece: resets the provider and makes this scenario's bearer.
    pub(super) fn start_mock_provider_piece(
        &mut self,
        alias: &str,
        wp: &crate::world::WorldPiece,
    ) -> Result<(), Fail> {
        let mocks = self.mocks()?;
        let kind = match wp.provider.as_deref() {
            Some("microsoft-login") => Kind::Login,
            Some("linear-server") => Kind::Linear,
            Some("microsoft-graph") => Kind::Graph,
            other => {
                return Err(Fail::new(
                    Class::Scenario,
                    "provider microsoft-login, microsoft-graph or linear-server",
                    format!("{other:?}"),
                ))
            }
        };
        let bearer = rand_token();
        {
            let mut st = mocks.state(kind).lock().unwrap();
            *st = State::default();
            if kind == Kind::Linear {
                st.bearer = bearer.clone();
            }
        }
        if kind == Kind::Linear {
            self.remember(alias, &bearer);
            self.ctx.conn.linear_bearer = bearer;
        }
        self.ctx.conn.providers.insert(
            alias.to_string(),
            match kind {
                Kind::Login => 0,
                Kind::Linear => 1,
                Kind::Graph => 2,
            },
        );
        Ok(())
    }

    pub(super) fn mock_provider(&mut self, m: &MockProviderStep) -> Result<(), Fail> {
        let kind = self.provider_of(&m.piece)?;
        let mocks = self.mocks()?;
        let mut st = mocks.state(kind).lock().unwrap();
        if let Some(h) = m.hold {
            if kind != Kind::Graph {
                return Err(Fail::new(
                    Class::Scenario,
                    "`hold` on a microsoft-graph mock-provider piece",
                    format!("piece `{}`", m.piece),
                ));
            }
            st.hold = h;
        } else {
            st.revoked = m.revoke;
        }
        Ok(())
    }

    /// Removes what the steps made, as the identity that made it.
    pub(super) fn cleanup_connectors(&mut self) {
        for (id, key) in std::mem::take(&mut self.ctx.conn.mcp) {
            let _ = self
                .api
                .delete_as(key.as_deref(), &format!("/v1/mcp/connections/{}", seg(&id)));
        }
        for (id, key) in std::mem::take(&mut self.ctx.conn.rest) {
            let _ = self.api.delete_as(
                key.as_deref(),
                &format!("/v1/connectors/{}/connection", seg(&id)),
            );
        }
        for key in std::mem::take(&mut self.ctx.conn.contact_keys) {
            let Ok(r) = self.api.get_as(key.as_deref(), "/v1/me/contacts") else {
                continue;
            };
            let ids: Vec<String> = r.json()["contacts"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c["id"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            for id in ids {
                let _ = self.api.post_as(
                    key.as_deref(),
                    &format!("/v1/me/contacts/{}/unlink", seg(&id)),
                    &json!({}),
                );
            }
        }
        // Node state outlives a scenario on one vamd: put every touched connector back to
        // "not enabled, no consent" so the next scenario starts clean.
        for id in std::mem::take(&mut self.ctx.conn.nodes) {
            let _ = self.api.put_as(
                None,
                &format!("/v1/connectors/{}/node", seg(&id)),
                &json!({ "enabled": false, "consent": null }),
            );
        }
        self.ctx.conn = ConnState::default();
    }

    /// A refusal must be 400, 403 or 409: a 404 or 405 is a missing route.
    fn judge(&self, step: &str, refused: bool, r: &crate::api::Reply) -> Result<(), Fail> {
        let shown = format!(
            "{} {}",
            r.status,
            self.scrub(&r.body.chars().take(300).collect::<String>())
        );
        if refused {
            if matches!(r.status, 400 | 403 | 409) {
                return Ok(());
            }
            return Err(product(
                format!("{step} is refused (400, 403 or 409)"),
                shown,
            ));
        }
        if r.ok() {
            Ok(())
        } else {
            Err(product(format!("{step} is accepted"), shown))
        }
    }

    pub(super) fn connector_node(&mut self, n: &ConnectorNode) -> Result<(), Fail> {
        let key = self.key_of(n.as_.as_deref())?;
        let mut body = json!({ "enabled": n.enabled });
        if let Some(c) = &n.consent {
            body["consent"] = json!({ "tenantId": c.tenant });
        }
        let path = format!("/v1/connectors/{}/node", seg(&n.id));
        self.ctx.conn.nodes.insert(n.id.clone());
        let r = self
            .api
            .put_as(key.as_deref(), &path, &body)
            .map_err(conn)?;
        self.judge(&format!("PUT {path}"), n.refused, &r)
    }

    pub(super) fn connector_check(&mut self, c: &ConnectorCheck) -> Result<(), Fail> {
        let key = self.key_of(c.as_.as_deref())?;
        let path = format!("/v1/connectors/{}/connection/check", seg(&c.id));
        let r = self
            .api
            .post_as(key.as_deref(), &path, &json!({}))
            .map_err(conn)?;
        self.judge(&format!("POST {path}"), false, &r)
    }

    pub(super) fn connector_connect(&mut self, c: &ConnectorConnect) -> Result<(), Fail> {
        let key = self.key_of(c.as_.as_deref())?;
        let kind = match &c.piece {
            Some(p) => Some(self.provider_of(p)?),
            None => None,
        };
        if c.id == "microsoft-365" {
            return self.connect_rest(c, key, kind);
        }
        let mocks = self.mocks()?;
        let url = match (&c.url, kind) {
            (Some(u), _) => u.clone(),
            // The definition's own URL: the node maps it to the mock at connect time (the seam),
            // and the `https://` rule still applies to what is sent.
            (None, Some(Kind::Linear)) => LINEAR_MCP_URL.to_string(),
            (None, Some(k)) => format!("http://{}/mcp", mocks.addr(k)),
            (None, None) => {
                return Err(Fail::new(
                    Class::Scenario,
                    "`url` or `piece` on an MCP connector-connect",
                    "neither",
                ))
            }
        };
        let origin = url
            .rsplit_once('/')
            .map(|x| x.0)
            .unwrap_or(&url)
            .to_string();
        let body = json!({
            "connectionId": c.id,
            "url": url,
            "authMode": "oauth",
            "oauth": {
                "resource": url,
                "authorizationServer": origin,
                "tokenEndpoint": format!("{origin}/oauth/token"),
                "clientId": "e2e-client",
                "registrationMethod": "dcr",
                "scopes": ["read"]
            }
        });
        let r = self
            .api
            .put_as(key.as_deref(), "/v1/mcp/connections", &body)
            .map_err(conn)?;
        if r.ok() {
            self.ctx.conn.mcp.push((c.id.clone(), key.clone()));
        }
        self.judge("PUT /v1/mcp/connections", c.refused, &r)?;
        if c.refused {
            return Ok(());
        }
        let access = match kind {
            Some(Kind::Linear) => self.ctx.conn.linear_bearer.clone(),
            _ => rand_token(),
        };
        if kind != Some(Kind::Linear) {
            self.remember(&c.id, &access);
        }
        let r = self
            .api
            .put_as(
                key.as_deref(),
                &format!("/v1/mcp/connections/{}/oauth/tokens", seg(&c.id)),
                &json!({ "accessToken": access, "refreshToken": rand_token() }),
            )
            .map_err(conn)?;
        self.judge("PUT /v1/mcp/connections/<id>/oauth/tokens", false, &r)
    }

    fn connect_rest(
        &mut self,
        c: &ConnectorConnect,
        key: Option<String>,
        kind: Option<Kind>,
    ) -> Result<(), Fail> {
        let tenant = c.tenant.clone().unwrap_or_default();
        let upn = c.upn.clone().unwrap_or_else(|| "member@e2e.example".into());
        let claims = json!({
            "tid": tenant,
            "preferred_username": upn,
            "iss": format!("https://login.microsoftonline.com/{tenant}/v2.0"),
        });
        // An unsigned id token: vamd reads the claims and checks no signature in this phase.
        let id_token = format!(
            "{}.{}.{}",
            b64url(br#"{"alg":"none"}"#),
            b64url(claims.to_string().as_bytes()),
            b64url(b"e2e")
        );
        let (access, refresh) = (rand_token(), rand_token());
        for v in [&id_token, &access, &refresh] {
            self.remember(&c.id, v);
        }
        // `secret-absent: { secret: <connector id> }` judges the access token and the refresh token.
        self.ctx
            .secrets
            .insert(c.id.clone(), Secret(access.clone()));
        self.ctx
            .secrets
            .insert(format!("{}#refresh", c.id), Secret(refresh.clone()));
        if kind == Some(Kind::Login) {
            let mocks = self.mocks()?;
            let mut st = mocks.login.lock().unwrap();
            st.refresh.insert(refresh.clone());
            // The graph mock compares bearers with these: a token that expires at once is
            // `stale` from the start, otherwise it is the one in force.
            if c.expires_in == Some(0) {
                st.stale.insert(access.clone());
            } else {
                st.current = access.clone();
            }
        }
        let mut conn_body =
            json!({ "accessToken": access, "refreshToken": refresh, "idToken": id_token });
        if let Some(n) = c.expires_in {
            conn_body["expiresIn"] = json!(n);
        }
        let path = format!("/v1/connectors/{}/connection", seg(&c.id));
        let r = self
            .api
            .put_as(key.as_deref(), &path, &conn_body)
            .map_err(conn)?;
        if r.ok() {
            self.ctx.conn.rest.push((c.id.clone(), key.clone()));
        }
        self.judge(&format!("PUT {path}"), c.refused, &r)
    }

    pub(super) fn expect_connectors(&mut self, x: &ConnectorsExpect) -> Result<(), Fail> {
        let key = self.key_of(x.as_.as_deref())?;
        let r = self
            .api
            .get_as(key.as_deref(), "/v1/connectors")
            .map_err(conn)?;
        if !r.ok() {
            return Err(product(
                "GET /v1/connectors answers",
                format!(
                    "{} {}",
                    r.status,
                    self.scrub(&r.body.chars().take(300).collect::<String>())
                ),
            ));
        }
        let body = r.json();
        // No token the runner made is in any listing.
        let leaked = self
            .ctx
            .secrets
            .values()
            .any(|s| s.0.len() >= 16 && r.body.contains(&s.0));
        if leaked {
            return Err(product(
                "no token value in the listing",
                "a runner-made token",
            ));
        }
        let list = body
            .get("connectors")
            .and_then(Value::as_array)
            .or_else(|| body.as_array())
            .cloned()
            .unwrap_or_default();
        let Some(entry) = list.iter().find(|e| {
            e["id"].as_str() == Some(x.id.as_str())
                || e["connectorId"].as_str() == Some(x.id.as_str())
        }) else {
            return Err(product(
                format!("connector `{}` is listed", x.id),
                self.scrub(&r.body.chars().take(300).collect::<String>()),
            ));
        };
        for (path, want) in &x.fields {
            let got = dotted(entry, path);
            let yaml: Value = serde_json::to_value(want).unwrap_or(Value::Null);
            if got != Some(&yaml) {
                return Err(product(
                    format!("`{}` {path} is {yaml}", x.id),
                    format!("{got:?}"),
                ));
            }
        }
        for path in &x.absent {
            if dotted(entry, path).is_some() {
                return Err(product(
                    format!("`{}` has no {path}", x.id),
                    "present".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// What a provider saw. A request that has not arrived yet is waited for (up to 8 s) unless
    /// the step wants none.
    pub(super) fn expect_mock_requests(&mut self, x: &MockRequestsExpect) -> Result<(), Fail> {
        let start = std::time::Instant::now();
        loop {
            match self.mock_requests_once(x) {
                Err(f)
                    if x.count != Some(0)
                        && f.class == Class::Product
                        && start.elapsed() < std::time::Duration::from_secs(8) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                r => return r,
            }
        }
    }

    fn mock_requests_once(&mut self, x: &MockRequestsExpect) -> Result<(), Fail> {
        let kind = self.provider_of(&x.piece)?;
        let mocks = self.mocks()?;
        let seen: Vec<Logged> = mocks
            .state(kind)
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|l| path_matches(&x.path, &l.path))
            .filter(|l| x.method.as_deref().is_none_or(|m| l.method == m))
            .cloned()
            .collect();
        let all: BTreeSet<&String> = seen.iter().flat_map(|l| l.fields.iter()).collect();
        let want = format!("{} saw requests at {}", x.piece, x.path);
        let none_wanted = x.count == Some(0);
        if (seen.is_empty() && !none_wanted) || x.count.is_some_and(|n| n != seen.len()) {
            return Err(product(
                format!(
                    "{want}{}",
                    x.count.map(|n| format!(" ({n})")).unwrap_or_default()
                ),
                format!("{} requests", seen.len()),
            ));
        }
        for h in &x.has {
            if !all.contains(h) {
                return Err(product(
                    format!("{want} with `{h}`"),
                    format!("fields {all:?}"),
                ));
            }
        }
        for l in &x.lacks {
            if all.contains(l) {
                return Err(product(
                    format!("{want} without `{l}`"),
                    "it was sent".to_string(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(addr: SocketAddr, path: &str, extra: &str, body: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        write!(
            s,
            "POST {path} HTTP/1.1\r\nHost: x\r\n{extra}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn login_serves_registered_refresh_tokens_and_logs_names_only() {
        let m = Mocks::start().unwrap();
        m.login.lock().unwrap().refresh.insert("r-secret".into());
        let ok = post(
            m.login_addr,
            "/t1/oauth2/v2.0/token",
            "",
            "grant_type=refresh_token&client_id=cid&refresh_token=r-secret&scope=a+offline_access",
        );
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        m.login.lock().unwrap().revoked = true;
        let bad = post(
            m.login_addr,
            "/t1/oauth2/v2.0/token",
            "",
            "refresh_token=r-secret",
        );
        assert!(bad.contains("invalid_grant"));
        let st = m.login.lock().unwrap();
        let f = &st.log[0].fields;
        assert!(f.contains("refresh_token") && f.contains("client_id=cid"));
        assert!(!f.iter().any(|x| x.contains("r-secret")));
        assert!(!f.contains("resource"));
    }

    #[test]
    fn linear_requires_the_bearer_and_lists_tools() {
        let m = Mocks::start().unwrap();
        m.linear.lock().unwrap().bearer = "b-secret".into();
        let rpc = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        assert!(post(m.linear_addr, "/mcp", "", rpc).starts_with("HTTP/1.1 401"));
        let ok = post(
            m.linear_addr,
            "/mcp",
            "Authorization: Bearer b-secret\r\n",
            rpc,
        );
        assert!(ok.contains("export_workspace") && ok.contains("save_issue"));
        let st = m.linear.lock().unwrap();
        assert!(st.log[1].fields.contains("authorization"));
        assert!(!st
            .log
            .iter()
            .any(|l| l.fields.iter().any(|f| f.contains("b-secret"))));
    }

    #[test]
    fn form_decode_handles_plus_and_percent() {
        assert_eq!(form_decode("a+b%2Fc%"), "a b/c%");
    }
}
