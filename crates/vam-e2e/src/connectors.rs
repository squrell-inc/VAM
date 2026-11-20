//! The steps for connectors and the two runner-hosted mock providers.
//!
//! Steps: `connector-node`, `connector-connect`, `connector-check`, `mock-provider`, `api-call`;
//! expects: `connectors` and `mock-requests`. The wire shapes are written down in `e2e/README.md` under
//! "Connectors".
//!
//! The providers are std-only loopback HTTP servers, started once per run and reset by each
//! `mock-provider` piece:
//! - `filebox` (a fictional Box-like service) serves one listener for three hosts: the MCP server
//!   (`POST /mcp`, `search_files` and `get_file_content`), RFC 9728 and RFC 8414 metadata, `GET /authorize`
//!   (the runner plays the browser) and `POST /token` and `/revoke`. The token endpoint is a
//!   confidential client (`client_secret_post`) with single-use rotating refresh tokens.
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
/// The MCP URL of the `filebox` connector in the `marketplace-community-connectors` fixture.
pub const FILEBOX_MCP_URL: &str = "https://mcp.filebox.example/mcp";
/// The origin of the `filebox` authorization server (its metadata names it).
pub const FILEBOX_AUTH_ORIGIN: &str = "https://auth.filebox.example";
/// Add the `oauth:<https origin>` key to the seam: the discovery fetch, the code exchange and the
/// refresh at `https://auth.filebox.example` are answered by the filebox mock. On since CN-5.2 (a
/// default vamd refuses the whole variable).
pub const PASS_OAUTH_SEAM: bool = true;
/// The client ids the filebox provider has registered start with this.
const FILEBOX_CLIENT_PREFIX: &str = "e2e-filebox-client";
/// The redirect URI the runner uses when it plays the app.
const APP_REDIRECT_URI: &str = "squrell://oauth";

/// The two companies the ticketdesk provider serves; each has its own loopback listener, because an
/// `oauth:` or `rest:` key maps one https origin to one `http://127.0.0.1:<port>` with no path and the
/// request reaches the mock with the loopback `Host`.
const TICKETDESK_COMPANIES: [&str; 2] = ["acme", "globex"];
/// The suffix of every ticketdesk company address in the `marketplace-community-connectors` fixture.
const TICKETDESK_SUFFIX: &str = "ticketdesk.example";
/// The client ids the ticketdesk provider has registered start with this.
const TICKETDESK_CLIENT_PREFIX: &str = "e2e-ticketdesk-client";
/// Add the `rest:<https origin>` keys to the seam: the egress gate answers
/// `<company>.ticketdesk.example` from the company's listener. On since CN-6.4 (a default vamd, and
/// one from before it, refuse the whole variable on an unknown key).
pub const PASS_REST_SEAM: bool = true;

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
    /// With `refused`: the refusal's answer body contains this text.
    pub contains: Option<String>,
    /// An org-supplied OAuth client: `{ clientId, clientSecret: { piece, which } }`. The secret's
    /// value comes from the filebox mock and is never printed.
    pub client: Option<NodeClient>,
    /// A company address: `{ subdomain }`, sent as `tenant: { subdomain }` in the body.
    pub tenant: Option<TenantStep>,
}

/// `tenant: { subdomain }` of `connector-node`: the text is sent as written (the node judges it).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantStep {
    pub subdomain: String,
}

/// `client: { clientId, clientSecret }` of `connector-node`: sent as `client: { clientId,
/// clientSecret }` in the body of `PUT /v1/connectors/{id}/node`. Without `clientSecret` the body
/// carries no secret.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeClient {
    pub client_id: String,
    pub client_secret: Option<SecretRef>,
}

/// `{ piece, which }`: a client secret of a filebox `mock-provider` piece.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    pub piece: String,
    #[serde(default)]
    pub which: SecretWhich,
}

/// `current`: the secret the provider accepts now. `rotated`: the same, after a `rotateSecret`
/// step (a scenario that names it before any rotation is a scenario error). `wrong`: a value the
/// provider never held.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SecretWhich {
    #[default]
    Current,
    Rotated,
    Wrong,
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
    /// With `refused`: the refusal's answer body contains this text.
    pub contains: Option<String>,
    /// Org-supplied client (a filebox `piece`): `exchange` (default) plays the app as D29 says:
    /// authorize at the pinned endpoint, then post code, verifier and redirect URI to
    /// `POST /v1/connectors/{id}/connection/oauth/code`. `tokens` plays an app that holds the
    /// tokens itself: `PUT /v1/mcp/connections`, then `…/oauth/tokens`.
    pub via: Option<Via>,
    /// `exchange`: the redirect URI sent (default `squrell://oauth`).
    pub redirect_uri: Option<String>,
    /// `tokens`: the token endpoint named in the connection (default: the node's pinned one).
    pub token_endpoint: Option<String>,
    /// `tokens`: the client id named in the connection (default: the node's).
    pub client_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    Exchange,
    Tokens,
}

/// `api-call: { method, path, as, refused, contains, absent }`: one request as an identity.
/// `GET` and `DELETE` only. Without `refused` the answer must be 2xx; with it 400, 403 or 409
/// (a 404 or 405 is a missing route) and the body holds `contains`. `absent` names a filebox
/// `mock-provider` piece: no form of its client secret or tokens is in the answer body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiCallStep {
    #[serde(default = "get_method")]
    pub method: String,
    pub path: String,
    #[serde(rename = "as")]
    pub as_: Option<String>,
    #[serde(default)]
    pub refused: bool,
    pub contains: Option<String>,
    pub absent: Option<String>,
}

fn get_method() -> String {
    "GET".to_string()
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MockProviderStep {
    pub piece: String,
    /// The provider refuses everything from now on (`invalid_grant` / 401).
    #[serde(default)]
    pub revoke: bool,
    /// Graph only: `true` makes `POST /v1.0/me/sendMail` log the request and then wait without
    /// answering; `false` releases it (and later requests answer at once).
    pub hold: Option<bool>,
    /// Filebox only: the current access token is refused from now on (until a refresh).
    #[serde(default)]
    pub expire: bool,
    /// Filebox only: `true` makes the token endpoint log a refresh request and then wait without
    /// answering; `false` releases the waiting requests.
    pub hold_token: Option<bool>,
    /// Filebox only: the provider's client secret changes; the old one is refused.
    #[serde(default)]
    pub rotate_secret: bool,
    /// Ticketdesk only: access tokens issued from now on live 60 s (`expires_in: 60`), inside the
    /// node's swap window, so every call refreshes first.
    #[serde(default)]
    pub short_tokens: bool,
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
    /// Only requests that carry this field are counted and judged (`grant_type=refresh_token`).
    pub with: Option<String>,
    /// At least this many requests (after `with`).
    #[serde(rename = "atLeast")]
    pub at_least: Option<usize>,
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
    /// Filebox: the provider's own state.
    fb: Filebox,
    /// Ticketdesk: the provider's own state.
    td: Ticketdesk,
}

/// What the ticketdesk provider remembers (one state for every company). Values are kept here and
/// never logged.
#[derive(Default)]
struct Ticketdesk {
    /// A value the provider never held (what a test sends as a client secret).
    wrong: String,
    /// Codes minted and not redeemed: code -> (client_id, redirect, challenge, company).
    codes: BTreeMap<String, (String, String, String, String)>,
    /// The access token each company's API accepts now.
    access: BTreeMap<String, String>,
    /// Access tokens that were valid once.
    stale: BTreeSet<String>,
    /// Refresh tokens issued and not yet spent: token -> company.
    live: BTreeMap<String, String>,
    /// Refresh tokens spent once.
    spent: BTreeSet<String>,
    /// New access tokens live 60 s.
    short: bool,
    /// Every token issued, in order, for the runner's scrub list.
    issued: Vec<String>,
}

/// What the filebox provider remembers. Values are kept here and never logged.
#[derive(Default)]
struct Filebox {
    /// The client secret the provider accepts now.
    secret: String,
    /// Secrets the provider held before a `rotateSecret`: refused, logged as `old`.
    old: BTreeSet<String>,
    /// A value the provider never held.
    wrong: String,
    /// Codes the authorize endpoint minted and nobody redeemed: `(client_id, redirect, challenge)`.
    codes: BTreeMap<String, (String, String, String)>,
    /// The access token the MCP server accepts now (empty: none).
    access: String,
    /// Access tokens that were valid once (logged as `authorization-expired`).
    stale: BTreeSet<String>,
    /// Refresh tokens issued and not yet spent.
    live: BTreeSet<String>,
    /// Refresh tokens spent once: presenting one again is `invalid_grant`.
    spent: BTreeSet<String>,
    /// The token endpoint waits on refresh requests.
    hold_token: bool,
    /// Every token the provider issued, in order, for the runner's scrub list.
    issued: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Login,
    Linear,
    Graph,
    Filebox,
    Ticketdesk,
}

/// The providers.
pub struct Mocks {
    login: Arc<Mutex<State>>,
    linear: Arc<Mutex<State>>,
    graph: Arc<Mutex<State>>,
    filebox: Arc<Mutex<State>>,
    ticketdesk: Arc<Mutex<State>>,
    /// `(company, listener)` for each of `TICKETDESK_COMPANIES`.
    ticketdesk_addrs: Vec<(String, SocketAddr)>,
    login_addr: SocketAddr,
    linear_addr: SocketAddr,
    graph_addr: SocketAddr,
    filebox_addr: SocketAddr,
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
        let l4 = TcpListener::bind("127.0.0.1:0")?;
        let filebox = Arc::new(Mutex::new(State::default()));
        let ticketdesk = Arc::new(Mutex::new(State::default()));
        let mut td_listeners = Vec::new();
        for c in TICKETDESK_COMPANIES {
            td_listeners.push((c.to_string(), TcpListener::bind("127.0.0.1:0")?));
        }
        let m = Mocks {
            ticketdesk_addrs: td_listeners
                .iter()
                .map(|(c, l)| Ok((c.clone(), l.local_addr()?)))
                .collect::<std::io::Result<Vec<_>>>()?,
            ticketdesk: ticketdesk.clone(),
            filebox_addr: l4.local_addr()?,
            filebox: filebox.clone(),
            login_addr: l1.local_addr()?,
            linear_addr: l2.local_addr()?,
            graph_addr: l3.local_addr()?,
            login: login.clone(),
            linear: linear.clone(),
            graph: graph.clone(),
        };
        serve(l1, login.clone(), Kind::Login);
        serve(l2, linear, Kind::Linear);
        serve(l4, filebox, Kind::Filebox);
        serve_graph(l3, graph, login);
        for (company, l) in td_listeners {
            serve_ticketdesk(l, ticketdesk.clone(), company);
        }
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
            m[format!("mcp:{FILEBOX_MCP_URL}")] =
                json!(format!("http://{}/mcp", self.filebox_addr));
        }
        if PASS_OAUTH_SEAM {
            m[format!("oauth:{FILEBOX_AUTH_ORIGIN}")] =
                json!(format!("http://{}", self.filebox_addr));
            // Discovery-free providers: the fixed endpoints of a company address (CN-6.2, CN-6.3).
            for (company, addr) in &self.ticketdesk_addrs {
                m[format!("oauth:https://{company}.{TICKETDESK_SUFFIX}")] =
                    json!(format!("http://{addr}"));
            }
        }
        if PASS_REST_SEAM {
            for (company, addr) in &self.ticketdesk_addrs {
                m[format!("rest:https://{company}.{TICKETDESK_SUFFIX}")] =
                    json!(format!("http://{addr}"));
            }
        }
        m.to_string()
    }

    /// The listener of a ticketdesk company (`acme` when the name is unknown).
    fn ticketdesk_addr(&self, company: &str) -> SocketAddr {
        self.ticketdesk_addrs
            .iter()
            .find(|(c, _)| c == company)
            .map(|(_, a)| *a)
            .unwrap_or(self.ticketdesk_addrs[0].1)
    }

    fn state(&self, k: Kind) -> &Arc<Mutex<State>> {
        match k {
            Kind::Login => &self.login,
            Kind::Linear => &self.linear,
            Kind::Graph => &self.graph,
            Kind::Filebox => &self.filebox,
            Kind::Ticketdesk => &self.ticketdesk,
        }
    }

    fn addr(&self, k: Kind) -> SocketAddr {
        match k {
            Kind::Login => self.login_addr,
            Kind::Linear => self.linear_addr,
            Kind::Graph => self.graph_addr,
            Kind::Filebox => self.filebox_addr,
            Kind::Ticketdesk => self.ticketdesk_addr("acme"),
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
        let l = login.lock().unwrap_or_else(|e| e.into_inner());
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
    state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .log
        .push(Logged {
            path: path.clone(),
            method: req.method.clone(),
            fields,
        });
    if tag != "current" {
        return respond(out, 401, "application/json", r#"{"error":"unauthorized"}"#);
    }
    let query = req.path.split_once('?').map(|x| x.1).unwrap_or_default();
    if req.method == "GET" && path == "/v1.0/me/messages" && query.contains("$search=") {
        if let Some(l) = state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .log
            .last_mut()
        {
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
                if !state.lock().unwrap_or_else(|e| e.into_inner()).hold {
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
        405 => "Method Not Allowed",
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
        Kind::Filebox => filebox(out, req, state),
        Kind::Graph => unreachable!("served by serve_graph"),
        Kind::Ticketdesk => unreachable!("served by serve_ticketdesk"),
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
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
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
        let st = state.lock().unwrap_or_else(|e| e.into_inner());
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
    state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .log
        .push(Logged {
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

/// Form fields of the filebox token endpoint whose value may be logged: none is a secret.
const FILEBOX_LOGGED_VALUES: &[&str] = &["client_id", "grant_type", "redirect_uri", "resource"];

fn sha256_b64url(s: &str) -> String {
    use sha2::{Digest, Sha256};
    b64url(&Sha256::digest(s.as_bytes()))
}

fn form_pairs(body: &str) -> BTreeMap<String, String> {
    body.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (form_decode(k), form_decode(v))
        })
        .collect()
}

fn log_filebox(
    state: &Arc<Mutex<State>>,
    req: &Req,
    path: &str,
    fields: BTreeSet<String>,
) -> usize {
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    st.log.push(Logged {
        path: path.to_string(),
        method: req.method.clone(),
        fields,
    });
    st.log.len() - 1
}

fn filebox_issue(st: &mut State) -> Value {
    let (access, refresh) = (rand_token(), rand_token());
    let old = std::mem::replace(&mut st.fb.access, access.clone());
    if !old.is_empty() {
        st.fb.stale.insert(old);
    }
    st.fb.live.insert(refresh.clone());
    st.fb.issued.push(access.clone());
    st.fb.issued.push(refresh.clone());
    json!({ "token_type": "Bearer", "access_token": access, "refresh_token": refresh, "expires_in": 3600 })
}

fn filebox(out: TcpStream, req: Req, state: &Arc<Mutex<State>>) {
    let (path, query) = req.path.split_once('?').unwrap_or((req.path.as_str(), ""));
    let path = path.to_string();
    let query = query.to_string();
    match (req.method.as_str(), path.as_str()) {
        ("GET", p) if p.starts_with("/.well-known/oauth-protected-resource") => {
            log_filebox(state, &req, &path, BTreeSet::new());
            let body = json!({
                "resource": FILEBOX_MCP_URL,
                "authorization_servers": [FILEBOX_AUTH_ORIGIN],
                "bearer_methods_supported": ["header"]
            });
            respond(out, 200, "application/json", &body.to_string());
        }
        (
            "GET",
            "/.well-known/oauth-authorization-server" | "/.well-known/openid-configuration",
        ) => {
            log_filebox(state, &req, &path, BTreeSet::new());
            // No registration endpoint: the admin registered a client by hand.
            let body = json!({
                "issuer": FILEBOX_AUTH_ORIGIN,
                "authorization_endpoint": format!("{FILEBOX_AUTH_ORIGIN}/authorize"),
                "token_endpoint": format!("{FILEBOX_AUTH_ORIGIN}/token"),
                "revocation_endpoint": format!("{FILEBOX_AUTH_ORIGIN}/revoke"),
                "response_types_supported": ["code"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["client_secret_post"]
            });
            respond(out, 200, "application/json", &body.to_string());
        }
        ("GET", "/authorize") => filebox_authorize(out, req, state, &query),
        ("POST", "/token") => filebox_token(out, req, state),
        ("POST", "/revoke") => {
            let names: BTreeSet<String> = form_pairs(&req.body).into_keys().collect();
            log_filebox(state, &req, &path, names);
            respond(out, 200, "application/json", "{}");
        }
        ("POST", "/mcp") => filebox_mcp(out, req, state),
        _ => respond(out, 404, "text/plain", "not found"),
    }
}

/// The browser's half: a code for the registered client id, the redirect URI and the S256
/// challenge, in the `Location` of a 302.
fn filebox_authorize(mut out: TcpStream, req: Req, state: &Arc<Mutex<State>>, query: &str) {
    let q = form_pairs(query);
    let get = |k: &str| q.get(k).cloned().unwrap_or_default();
    let mut fields: BTreeSet<String> = q.keys().cloned().collect();
    for k in ["client_id", "code_challenge_method", "redirect_uri"] {
        fields.insert(format!("{k}={}", get(k)));
    }
    log_filebox(state, &req, "/authorize", fields);
    let ok = get("response_type") == "code"
        && get("client_id").starts_with(FILEBOX_CLIENT_PREFIX)
        && get("code_challenge_method") == "S256"
        && !get("code_challenge").is_empty()
        && !get("redirect_uri").is_empty();
    if !ok {
        return respond(out, 400, "text/plain", "invalid_request");
    }
    let code = rand_token();
    state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .fb
        .codes
        .insert(
            code.clone(),
            (get("client_id"), get("redirect_uri"), get("code_challenge")),
        );
    let sep = if get("redirect_uri").contains('?') {
        '&'
    } else {
        '?'
    };
    let _ = write!(
        out,
        "HTTP/1.1 302 Found\r\nLocation: {}{sep}code={code}&state={}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        get("redirect_uri"),
        get("state")
    );
}

/// The confidential token endpoint: `client_secret_post`, single-use codes, single-use rotating
/// refresh tokens. Logs field names, a few non-secret values, `client-secret=current|old|wrong|none`
/// and, for a refresh, `refresh=fresh|reused|unknown`; never a value.
fn filebox_token(out: TcpStream, req: Req, state: &Arc<Mutex<State>>) {
    let form = form_pairs(&req.body);
    let grant = form.get("grant_type").cloned().unwrap_or_default();
    let client_id = form.get("client_id").cloned().unwrap_or_default();
    let label_of = |st: &State| match form.get("client_secret") {
        None => "none",
        Some(s) if *s == st.fb.secret => "current",
        Some(s) if st.fb.old.contains(s) => "old",
        Some(_) => "wrong",
    };
    // What the request arrived with, for the log. The grant is decided on what is current when it
    // is consumed, after a hold.
    let secret_label = label_of(&state.lock().unwrap_or_else(|e| e.into_inner()));
    let mut fields: BTreeSet<String> = form.keys().cloned().collect();
    for k in FILEBOX_LOGGED_VALUES {
        if let Some(v) = form.get(*k) {
            fields.insert(format!("{k}={v}"));
        }
    }
    fields.insert(format!("client-secret={secret_label}"));
    let idx = log_filebox(state, &req, "/token", fields);
    // A held refresh waits, logged, until released (at most 30 s: a stuck scenario still ends).
    if grant == "refresh_token" {
        let start = std::time::Instant::now();
        while state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fb
            .hold_token
            && start.elapsed() < std::time::Duration::from_secs(30)
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    let mut note = None;
    let result: Result<Value, (u16, &str)> = {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        if label_of(&st) != "current" || !client_id.starts_with(FILEBOX_CLIENT_PREFIX) {
            Err((401, "invalid_client"))
        } else if st.revoked && grant == "refresh_token" {
            // A revoked grant is refused before it is consumed.
            note = Some("refresh=revoked");
            Err((400, "invalid_grant"))
        } else if grant == "authorization_code" {
            let code = form.get("code").map(String::as_str).unwrap_or_default();
            match st.fb.codes.remove(code) {
                Some((cid, redirect, challenge))
                    if cid == client_id
                        && form.get("redirect_uri") == Some(&redirect)
                        && sha256_b64url(
                            form.get("code_verifier")
                                .map(String::as_str)
                                .unwrap_or_default(),
                        ) == challenge =>
                {
                    Ok(filebox_issue(&mut st))
                }
                _ => Err((400, "invalid_grant")),
            }
        } else if grant == "refresh_token" {
            let rt = form.get("refresh_token").cloned().unwrap_or_default();
            if st.fb.live.remove(&rt) {
                st.fb.spent.insert(rt);
                note = Some("refresh=fresh");
                Ok(filebox_issue(&mut st))
            } else if st.fb.spent.contains(&rt) {
                note = Some("refresh=reused");
                Err((400, "invalid_grant"))
            } else {
                note = Some("refresh=unknown");
                Err((400, "invalid_grant"))
            }
        } else {
            Err((400, "unsupported_grant_type"))
        }
    };
    if let Some(n) = note {
        if let Some(l) = state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .log
            .get_mut(idx)
        {
            l.fields.insert(n.to_string());
        }
    }
    match result {
        Ok(v) => respond(out, 200, "application/json", &v.to_string()),
        Err((status, err)) => respond(
            out,
            status,
            "application/json",
            &json!({ "error": err }).to_string(),
        ),
    }
}

fn filebox_mcp(out: TcpStream, req: Req, state: &Arc<Mutex<State>>) {
    let rpc: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
    let method = rpc["method"].as_str().unwrap_or_default().to_string();
    let id = rpc["id"].clone();
    let sent = req
        .headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let mut fields = BTreeSet::new();
    fields.insert(format!("method={method}"));
    let mut expired = false;
    let authed = {
        let st = state.lock().unwrap_or_else(|e| e.into_inner());
        let authed =
            !st.revoked && !st.fb.access.is_empty() && sent.as_deref() == Some(&st.fb.access);
        match &sent {
            None => {}
            Some(_) if authed => {
                fields.insert("authorization".to_string());
            }
            Some(t) if st.fb.stale.contains(t) => {
                expired = true;
                fields.insert("authorization-expired".to_string());
            }
            Some(_) => {
                fields.insert("authorization-wrong".to_string());
            }
        }
        authed
    };
    if method == "tools/call" {
        let tool = rpc["params"]["name"].as_str().unwrap_or_default();
        fields.insert(format!("tool={tool}"));
        // `tool-ok=` only for a call the provider answered.
        if authed {
            fields.insert(format!("tool-ok={tool}"));
        }
    }
    log_filebox(state, &req, "/mcp", fields);
    if !authed {
        // A held provider is slow on a refused call too: the request is logged on arrival and
        // answered when the hold is released (at most 30 s). The node makes a caller that starts
        // while a refresh is in flight wait for it before it sends, so this is what keeps two tasks
        // in flight on the same expired token at once.
        if expired {
            let start = std::time::Instant::now();
            while state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .fb
                .hold_token
                && start.elapsed() < std::time::Duration::from_secs(30)
            {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        return respond(out, 401, "application/json", r#"{"error":"unauthorized"}"#);
    }
    let result = match method.as_str() {
        "initialize" => json!({
            "protocolVersion": "2025-03-26",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mock-filebox", "version": "1" }
        }),
        "tools/list" => {
            let tool = |name: &str| {
                json!({ "name": name, "description": name,
                        "inputSchema": { "type": "object", "properties": {} } })
            };
            json!({ "tools": [tool("search_files"), tool("get_file_content")] })
        }
        "tools/call" => json!({ "content": [{ "type": "text", "text": "ok" }], "isError": false }),
        _ => return respond(out, 202, "application/json", ""),
    };
    let body = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    respond(out, 200, "application/json", &body.to_string());
}

fn serve_ticketdesk(listener: TcpListener, state: Arc<Mutex<State>>, company: String) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (state, company) = (state.clone(), company.clone());
            std::thread::spawn(move || {
                let Ok(req) = read_req(&stream) else { return };
                let Ok(out) = stream.try_clone() else { return };
                ticketdesk(out, req, &state, &company);
            });
        }
    });
}

/// Logs a ticketdesk request: `host=<company>` and the given fields; returns the log index.
fn log_ticketdesk(
    state: &Arc<Mutex<State>>,
    req: &Req,
    path: &str,
    company: &str,
    mut fields: BTreeSet<String>,
) -> usize {
    fields.insert(format!("host={company}"));
    log_filebox(state, req, path, fields)
}

fn ticketdesk_issue(st: &mut State, company: &str) -> Value {
    let (access, refresh) = (rand_token(), rand_token());
    if let Some(old) = st.td.access.insert(company.to_string(), access.clone()) {
        st.td.stale.insert(old);
    }
    st.td.live.insert(refresh.clone(), company.to_string());
    st.td.issued.push(access.clone());
    st.td.issued.push(refresh.clone());
    let ttl = if st.td.short { 60 } else { 3600 };
    json!({ "token_type": "Bearer", "access_token": access, "refresh_token": refresh, "expires_in": ttl })
}

/// A Zendesk-like service at `<company>.ticketdesk.example`: fixed OAuth endpoints (no metadata), a
/// public client at the token endpoint and a REST API that needs the current bearer for that company.
fn ticketdesk(out: TcpStream, req: Req, state: &Arc<Mutex<State>>, company: &str) {
    let (path, query) = req.path.split_once('?').unwrap_or((req.path.as_str(), ""));
    let (path, query) = (path.to_string(), query.to_string());
    match (req.method.as_str(), path.as_str()) {
        ("GET", "/oauth/authorizations/new") => {
            ticketdesk_authorize(out, req, state, company, &query)
        }
        ("POST", "/oauth/tokens") => ticketdesk_token(out, req, state, company),
        ("GET", "/api/v2/search.json") => {
            let body = json!({ "results": [{ "id": 101, "subject": "Printer on fire", "status": "open" }], "count": 1 });
            ticketdesk_api(out, req, state, company, &path, 200, body)
        }
        ("GET", p) if p.starts_with("/api/v2/tickets/") && p.ends_with(".json") => {
            let id = p["/api/v2/tickets/".len()..p.len() - ".json".len()].to_string();
            let body =
                json!({ "ticket": { "id": id, "subject": "Printer on fire", "status": "open" } });
            ticketdesk_api(out, req, state, company, &path, 200, body)
        }
        ("POST", "/api/v2/tickets.json") => {
            let body = json!({ "ticket": { "id": 102, "subject": "Created by the agent", "status": "new" } });
            ticketdesk_api(out, req, state, company, &path, 201, body)
        }
        _ => {
            // An unmatched request is logged too (method, host and path), so a scenario that says
            // the provider saw no request at all (`path: "*", count: 0`) also catches discovery
            // probes such as `/.well-known/oauth-authorization-server`.
            let mut fields = BTreeSet::new();
            fields.insert(format!("method={}", req.method));
            log_ticketdesk(state, &req, &path, company, fields);
            respond(out, 404, "text/plain", "not found")
        }
    }
}

/// The browser's half: a code for a registered client id, the redirect URI and the S256 challenge,
/// in the `Location` of a 302, bound to this company.
fn ticketdesk_authorize(
    mut out: TcpStream,
    req: Req,
    state: &Arc<Mutex<State>>,
    company: &str,
    query: &str,
) {
    let q = form_pairs(query);
    let get = |k: &str| q.get(k).cloned().unwrap_or_default();
    let mut fields: BTreeSet<String> = q.keys().cloned().collect();
    for k in ["client_id", "code_challenge_method", "redirect_uri"] {
        fields.insert(format!("{k}={}", get(k)));
    }
    log_ticketdesk(state, &req, "/oauth/authorizations/new", company, fields);
    let ok = get("response_type") == "code"
        && get("client_id").starts_with(TICKETDESK_CLIENT_PREFIX)
        && get("code_challenge_method") == "S256"
        && !get("code_challenge").is_empty()
        && !get("redirect_uri").is_empty();
    if !ok {
        return respond(out, 400, "text/plain", "invalid_request");
    }
    let code = rand_token();
    state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .td
        .codes
        .insert(
            code.clone(),
            (
                get("client_id"),
                get("redirect_uri"),
                get("code_challenge"),
                company.to_string(),
            ),
        );
    let sep = if get("redirect_uri").contains('?') {
        '&'
    } else {
        '?'
    };
    let _ = write!(
        out,
        "HTTP/1.1 302 Found\r\nLocation: {}{sep}code={code}&state={}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        get("redirect_uri"),
        get("state")
    );
}

/// A public client: no `client_secret` (a request that has one is refused), single-use codes
/// checked with verifier and redirect URI, single-use rotating refresh tokens. Logs field names, a
/// few non-secret values, `client-secret=none|present`, `resource=none|present` and, for a refresh,
/// `refresh=fresh|reused|unknown`; never a value.
fn ticketdesk_token(out: TcpStream, req: Req, state: &Arc<Mutex<State>>, company: &str) {
    let form = form_pairs(&req.body);
    let grant = form.get("grant_type").cloned().unwrap_or_default();
    let client_id = form.get("client_id").cloned().unwrap_or_default();
    let mut fields: BTreeSet<String> = form.keys().cloned().collect();
    for k in ["client_id", "grant_type", "redirect_uri"] {
        if let Some(v) = form.get(k) {
            fields.insert(format!("{k}={v}"));
        }
    }
    let has = |k: &str, yes: &'static str, no: &'static str| {
        if form.contains_key(k) {
            yes
        } else {
            no
        }
    };
    fields.insert(format!(
        "client-secret={}",
        has("client_secret", "present", "none")
    ));
    fields.insert(format!("resource={}", has("resource", "present", "none")));
    let idx = log_ticketdesk(state, &req, "/oauth/tokens", company, fields);
    let mut note = None;
    let result: Result<Value, (u16, &str)> = {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        if form.contains_key("client_secret") || !client_id.starts_with(TICKETDESK_CLIENT_PREFIX) {
            Err((401, "invalid_client"))
        } else if st.revoked && grant == "refresh_token" {
            note = Some("refresh=revoked");
            Err((400, "invalid_grant"))
        } else if grant == "authorization_code" {
            let code = form.get("code").map(String::as_str).unwrap_or_default();
            match st.td.codes.remove(code) {
                Some((cid, redirect, challenge, owner))
                    if cid == client_id
                        && owner == company
                        && form.get("redirect_uri") == Some(&redirect)
                        && sha256_b64url(
                            form.get("code_verifier")
                                .map(String::as_str)
                                .unwrap_or_default(),
                        ) == challenge =>
                {
                    Ok(ticketdesk_issue(&mut st, company))
                }
                _ => Err((400, "invalid_grant")),
            }
        } else if grant == "refresh_token" {
            let rt = form.get("refresh_token").cloned().unwrap_or_default();
            if st.td.live.get(&rt).is_some_and(|c| c == company) {
                st.td.live.remove(&rt);
                st.td.spent.insert(rt);
                note = Some("refresh=fresh");
                Ok(ticketdesk_issue(&mut st, company))
            } else if st.td.spent.contains(&rt) {
                note = Some("refresh=reused");
                Err((400, "invalid_grant"))
            } else {
                note = Some("refresh=unknown");
                Err((400, "invalid_grant"))
            }
        } else {
            Err((400, "unsupported_grant_type"))
        }
    };
    if let Some(n) = note {
        if let Some(l) = state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .log
            .get_mut(idx)
        {
            l.fields.insert(n.to_string());
        }
    }
    match result {
        Ok(v) => respond(out, 200, "application/json", &v.to_string()),
        Err((status, err)) => respond(
            out,
            status,
            "application/json",
            &json!({ "error": err }).to_string(),
        ),
    }
}

/// The REST API: 401 unless the bearer is the current access token of this company. Logs
/// `method=` and `bearer=current|stale|placeholder|unknown|none`, never a value.
fn ticketdesk_api(
    out: TcpStream,
    req: Req,
    state: &Arc<Mutex<State>>,
    company: &str,
    path: &str,
    status: u16,
    body: Value,
) {
    let sent = req
        .headers
        .get("authorization")
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).to_string());
    let (label, authed) = {
        let st = state.lock().unwrap_or_else(|e| e.into_inner());
        match &sent {
            None => ("none", false),
            Some(t) if t.starts_with("vamsec_") => ("placeholder", false),
            Some(t) if !st.revoked && st.td.access.get(company) == Some(t) => ("current", true),
            Some(t) if st.td.stale.contains(t) => ("stale", false),
            Some(_) => ("unknown", false),
        }
    };
    let mut fields = BTreeSet::new();
    fields.insert(format!("method={}", req.method));
    fields.insert(format!("bearer={label}"));
    log_ticketdesk(state, &req, path, company, fields);
    if !authed {
        return respond(out, 401, "application/json", r#"{"error":"unauthorized"}"#);
    }
    respond(out, status, "application/json", &body.to_string());
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

/// The browser's half of the authorization-code flow: `GET` the authorize URL on the mock and read
/// the `code` out of the `Location` of the redirect.
fn browser_get_code(addr: SocketAddr, path_and_query: &str) -> Result<String, String> {
    let mut s = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    write!(
        s,
        "GET {path_and_query} HTTP/1.1\r\nHost: auth.filebox.example\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut out = String::new();
    s.read_to_string(&mut out).map_err(|e| e.to_string())?;
    let status = out.lines().next().unwrap_or_default().to_string();
    let location = out
        .lines()
        .find_map(|l| l.strip_prefix("Location: "))
        .ok_or_else(|| format!("no redirect: {status}"))?;
    let query = location.split_once('?').map(|x| x.1).unwrap_or_default();
    form_pairs(query)
        .remove("code")
        .ok_or_else(|| "the redirect carries no code".to_string())
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
    /// Alias of the filebox `mock-provider` piece of this scenario.
    filebox_alias: Option<String>,
    /// How many of the filebox provider's issued tokens the scrub list already holds.
    filebox_synced: usize,
    /// Connector ids a `connector-node` step gave a client; cleared at cleanup.
    client_nodes: std::collections::BTreeSet<String>,
    /// Connector ids a `connector-node` step gave a company address (accepted); cleared at cleanup.
    tenant_nodes: std::collections::BTreeSet<String>,
    /// Alias of the ticketdesk `mock-provider` piece of this scenario.
    ticketdesk_alias: Option<String>,
    /// How many of the ticketdesk provider's issued tokens the scrub list already holds.
    ticketdesk_synced: usize,
}

fn product(want: impl Into<String>, got: impl Into<String>) -> Fail {
    Fail::new(Class::Product, want, got)
}

/// The cleanup body that puts a touched connector back to "not enabled, no consent": `client:
/// null` for a connector whose client the scenario saved, and `tenant: null` for one whose
/// company address it saved, each on its own (the API keeps either when it is omitted).
fn node_reset_body(client: bool, tenant: bool) -> Value {
    let mut body = json!({ "enabled": false, "consent": null });
    if client {
        body["client"] = Value::Null;
    }
    if tenant {
        body["tenant"] = Value::Null;
    }
    body
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
            Some(3) => Ok(Kind::Filebox),
            Some(4) => Ok(Kind::Ticketdesk),
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
        if wp.provider.as_deref() == Some("filebox")
            && self
                .ctx
                .conn
                .filebox_alias
                .as_deref()
                .is_some_and(|first| first != alias)
        {
            // One listener, one provider state: a second piece would reset it and take over the
            // token bookkeeping of the first.
            return Err(Fail::new(
                Class::Scenario,
                "one filebox mock-provider piece per scenario",
                format!("`{alias}` is a second one"),
            ));
        }
        if wp.provider.as_deref() == Some("ticketdesk")
            && self
                .ctx
                .conn
                .ticketdesk_alias
                .as_deref()
                .is_some_and(|first| first != alias)
        {
            return Err(Fail::new(
                Class::Scenario,
                "one ticketdesk mock-provider piece per scenario",
                format!("`{alias}` is a second one"),
            ));
        }
        let kind = match wp.provider.as_deref() {
            Some("ticketdesk") => Kind::Ticketdesk,
            Some("microsoft-login") => Kind::Login,
            Some("linear-server") => Kind::Linear,
            Some("microsoft-graph") => Kind::Graph,
            Some("filebox") => Kind::Filebox,
            other => {
                return Err(Fail::new(
                    Class::Scenario,
                    "provider microsoft-login, microsoft-graph, linear-server or filebox",
                    format!("{other:?}"),
                ))
            }
        };
        let bearer = rand_token();
        {
            let mut st = mocks.state(kind).lock().unwrap_or_else(|e| e.into_inner());
            *st = State::default();
            if kind == Kind::Linear {
                st.bearer = bearer.clone();
            }
            if kind == Kind::Filebox {
                st.fb.secret = rand_token();
                st.fb.wrong = rand_token();
            }
            if kind == Kind::Ticketdesk {
                st.td.wrong = rand_token();
            }
        }
        if kind == Kind::Ticketdesk {
            // The value a test sends as a client secret (the provider has none) and, later, every
            // token it issues: `secret-absent: { secret: <alias> }` judges them all.
            let wrong = mocks
                .state(kind)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .td
                .wrong
                .clone();
            self.ctx.secrets.insert(alias.to_string(), Secret(wrong));
            self.ctx.conn.ticketdesk_alias = Some(alias.to_string());
            self.ctx.conn.ticketdesk_synced = 0;
        }
        if kind == Kind::Filebox {
            // The client secret the provider holds, a value it never held, and later the rotated
            // one and every token it issues: all are in the scrub list, and `secret-absent:
            // { secret: <alias> }` judges them all (the companions are `<alias>#...`).
            let (secret, wrong) = {
                let st = mocks.state(kind).lock().unwrap_or_else(|e| e.into_inner());
                (st.fb.secret.clone(), st.fb.wrong.clone())
            };
            self.ctx.secrets.insert(alias.to_string(), Secret(secret));
            self.ctx
                .secrets
                .insert(format!("{alias}#wrong"), Secret(wrong));
            self.ctx.conn.filebox_alias = Some(alias.to_string());
            self.ctx.conn.filebox_synced = 0;
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
                Kind::Filebox => 3,
                Kind::Ticketdesk => 4,
            },
        );
        Ok(())
    }

    pub(super) fn mock_provider(&mut self, m: &MockProviderStep) -> Result<(), Fail> {
        let kind = self.provider_of(&m.piece)?;
        let mocks = self.mocks()?;
        if m.short_tokens {
            if kind != Kind::Ticketdesk
                || m.expire
                || m.hold_token.is_some()
                || m.rotate_secret
                || m.revoke
                || m.hold.is_some()
            {
                return Err(Fail::new(
                    Class::Scenario,
                    "`shortTokens` alone on a ticketdesk mock-provider piece",
                    format!("piece `{}`", m.piece),
                ));
            }
            mocks
                .state(kind)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .td
                .short = true;
            return Ok(());
        }
        let filebox_only = m.expire || m.hold_token.is_some() || m.rotate_secret;
        if filebox_only && kind != Kind::Filebox {
            return Err(Fail::new(
                Class::Scenario,
                "`expire`, `holdToken` and `rotateSecret` on a filebox mock-provider piece",
                format!("piece `{}`", m.piece),
            ));
        }
        if filebox_only && m.revoke {
            // `revoke` is a standalone control: combined with the others it would be dropped.
            return Err(Fail::new(
                Class::Scenario,
                "`revoke` alone on a mock-provider step",
                format!(
                    "piece `{}`: `revoke` is not combined with `expire`, `holdToken` or `rotateSecret`",
                    m.piece
                ),
            ));
        }
        let mut st = mocks.state(kind).lock().unwrap_or_else(|e| e.into_inner());
        if filebox_only {
            if m.expire {
                let cur = std::mem::take(&mut st.fb.access);
                if !cur.is_empty() {
                    st.fb.stale.insert(cur);
                }
            }
            if let Some(h) = m.hold_token {
                st.fb.hold_token = h;
            }
            if m.rotate_secret {
                let old = std::mem::replace(&mut st.fb.secret, rand_token());
                st.fb.old.insert(old);
                let new = st.fb.secret.clone();
                drop(st);
                // Every rotated secret stays in the leak-check set: the one before this rotation
                // moves to `<alias>#rotated-<n>`, and `<alias>#rotated` is always the latest.
                let latest = format!("{}#rotated", m.piece);
                if let Some(prev) = self.ctx.secrets.remove(&latest) {
                    let earlier = self
                        .ctx
                        .secrets
                        .keys()
                        .filter(|k| k.starts_with(&format!("{latest}-")))
                        .count();
                    self.ctx
                        .secrets
                        .insert(format!("{latest}-{}", earlier + 1), prev);
                }
                self.ctx.secrets.insert(latest, Secret(new));
            }
            return Ok(());
        }
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
        let client_nodes = std::mem::take(&mut self.ctx.conn.client_nodes);
        let tenant_nodes = std::mem::take(&mut self.ctx.conn.tenant_nodes);
        for id in std::mem::take(&mut self.ctx.conn.nodes) {
            let path = format!("/v1/connectors/{}/node", seg(&id));
            let (client, tenant) = (client_nodes.contains(&id), tenant_nodes.contains(&id));
            let answer = self
                .api
                .put_as(None, &path, &node_reset_body(client, tenant));
            if answer.as_ref().is_ok_and(|r| r.ok()) {
                continue;
            }
            let why = self.reset_failure(&answer);
            if client {
                // The client (and its vault secret) stays on this vamd: say so, as the marketplace
                // cleanup does, so a later scenario that finds it is not a mystery.
                eprintln!(
                    "vam-e2e: connector cleanup: the client of `{id}` could not be cleared ({why})"
                );
            }
            // Retry without the client, keeping the address reset: the API keeps an address
            // when `tenant` is omitted, so the next scenario would start with this one's.
            let answer = self
                .api
                .put_as(None, &path, &node_reset_body(false, tenant));
            if tenant && !answer.as_ref().is_ok_and(|r| r.ok()) {
                let why = self.reset_failure(&answer);
                eprintln!(
                    "vam-e2e: connector cleanup: the company address of `{id}` could not be cleared ({why})"
                );
            }
        }
        self.ctx.conn = ConnState::default();
    }

    /// The status and scrubbed start of a failed cleanup answer.
    fn reset_failure(&self, answer: &Result<crate::api::Reply, crate::api::ConnError>) -> String {
        match answer {
            Ok(r) => format!(
                "{} {}",
                r.status,
                self.scrub(&r.body.chars().take(200).collect::<String>())
            ),
            Err(_) => "no answer".to_string(),
        }
    }

    /// A refusal must be 400, 403 or 409: a 404 or 405 is a missing route.
    fn judge(&self, step: &str, refused: bool, r: &crate::api::Reply) -> Result<(), Fail> {
        self.judge_with(step, refused, None, r)
    }

    /// As `judge`, and a refusal's body must hold `contains`.
    fn judge_with(
        &self,
        step: &str,
        refused: bool,
        contains: Option<&str>,
        r: &crate::api::Reply,
    ) -> Result<(), Fail> {
        let shown = format!(
            "{} {}",
            r.status,
            self.scrub(&r.body.chars().take(300).collect::<String>())
        );
        if refused {
            if !matches!(r.status, 400 | 403 | 409) {
                return Err(product(
                    format!("{step} is refused (400, 403 or 409)"),
                    shown,
                ));
            }
            if let Some(c) = contains.filter(|c| !r.body.contains(*c)) {
                return Err(product(format!("{step} is refused with `{c}`"), shown));
            }
            return Ok(());
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
        if let Some(c) = &n.client {
            let mut client = json!({ "clientId": c.client_id });
            if let Some(s) = &c.client_secret {
                client["clientSecret"] = json!(self.client_secret_value(s)?);
            }
            body["client"] = client;
        }
        if let Some(t) = &n.tenant {
            body["tenant"] = json!({ "subdomain": t.subdomain });
        }
        let path = format!("/v1/connectors/{}/node", seg(&n.id));
        self.ctx.conn.nodes.insert(n.id.clone());
        let r = self
            .api
            .put_as(key.as_deref(), &path, &body)
            .map_err(conn)?;
        // Only a save the node accepted leaves a client (and its secret) to clear afterwards.
        if n.client.is_some() && r.ok() {
            self.ctx.conn.client_nodes.insert(n.id.clone());
        }
        if n.tenant.is_some() && r.ok() {
            self.ctx.conn.tenant_nodes.insert(n.id.clone());
        }
        self.judge_with(&format!("PUT {path}"), n.refused, n.contains.as_deref(), &r)
    }

    /// The value of a filebox client secret a `connector-node` step names.
    fn client_secret_value(&mut self, s: &SecretRef) -> Result<String, Fail> {
        let kind = self.provider_of(&s.piece)?;
        if kind == Kind::Ticketdesk {
            // The provider has no secret: whatever is sent is a value it never held.
            let mocks = self.mocks()?;
            let st = mocks
                .state(Kind::Ticketdesk)
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            return Ok(st.td.wrong.clone());
        }
        if kind != Kind::Filebox {
            return Err(Fail::new(
                Class::Scenario,
                "a filebox or ticketdesk mock-provider piece for `clientSecret.piece`",
                format!("piece `{}`", s.piece),
            ));
        }
        let mocks = self.mocks()?;
        let st = mocks
            .state(Kind::Filebox)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match s.which {
            SecretWhich::Current => Ok(st.fb.secret.clone()),
            SecretWhich::Rotated if st.fb.old.is_empty() => Err(Fail::new(
                Class::Scenario,
                "a `rotateSecret` step before `which: rotated`",
                "the provider's secret was never rotated",
            )),
            SecretWhich::Rotated => Ok(st.fb.secret.clone()),
            SecretWhich::Wrong => Ok(st.fb.wrong.clone()),
        }
    }

    /// Puts the tokens the filebox provider issued since the last call into the scrub list, as
    /// companions of its piece (`<alias>#t<n>`).
    pub(super) fn sync_filebox(&mut self) {
        if let (Some(alias), Some(mocks)) = (
            self.ctx.conn.ticketdesk_alias.clone(),
            self.connector_mocks.clone(),
        ) {
            let issued: Vec<String> = mocks
                .ticketdesk
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .td
                .issued
                .clone();
            for (n, t) in issued
                .iter()
                .enumerate()
                .skip(self.ctx.conn.ticketdesk_synced)
            {
                self.ctx
                    .secrets
                    .insert(format!("{alias}#t{n}"), Secret(t.clone()));
            }
            self.ctx.conn.ticketdesk_synced = issued.len();
        }
        let (Some(alias), Some(mocks)) = (
            self.ctx.conn.filebox_alias.clone(),
            self.connector_mocks.clone(),
        ) else {
            return;
        };
        let issued: Vec<String> = mocks
            .filebox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fb
            .issued
            .clone();
        for (n, t) in issued.iter().enumerate().skip(self.ctx.conn.filebox_synced) {
            self.ctx
                .secrets
                .insert(format!("{alias}#t{n}"), Secret(t.clone()));
        }
        self.ctx.conn.filebox_synced = issued.len();
    }

    /// A connector with an org-supplied client, connected as the app does (D29): the pinned
    /// authorize endpoint and client id come from the member's `GET /v1/connectors` entry.
    fn connect_org_client(
        &mut self,
        c: &ConnectorConnect,
        key: Option<String>,
        ticketdesk: bool,
    ) -> Result<(), Fail> {
        let mocks = self.mocks()?;
        let r = self
            .api
            .get_as(key.as_deref(), "/v1/connectors")
            .map_err(conn)?;
        let body = r.json();
        let entry = body["connectors"]
            .as_array()
            .and_then(|a| a.iter().find(|e| e["id"].as_str() == Some(c.id.as_str())))
            .cloned()
            .ok_or_else(|| {
                product(
                    format!("connector `{}` is listed", c.id),
                    self.scrub(&r.body.chars().take(300).collect::<String>()),
                )
            })?;
        let text = |path: &str| -> Result<String, Fail> {
            dotted(&entry, path)
                .and_then(Value::as_str)
                .map(String::from)
                .ok_or_else(|| {
                    product(
                        format!("`{}` has {path} (the node's pinned client)", c.id),
                        format!("{:?}", dotted(&entry, path)),
                    )
                })
        };
        let client_id = text("node.client.clientId")?;
        let authorize = text("node.client.authorizeEndpoint")?;
        let token_endpoint = text("node.client.tokenEndpoint")?;
        // A REST connector (ticketdesk) has no MCP server.
        let mcp_url = if ticketdesk {
            String::new()
        } else {
            text("mcp.url")?
        };
        if c.via == Some(Via::Tokens) && ticketdesk {
            // An app that holds the tokens itself: the REST connection route, which an
            // admin-supplied client's connector must refuse (the node exchanges the code).
            let (access, refresh) = (rand_token(), rand_token());
            self.remember(&c.id, &access);
            self.remember(&c.id, &refresh);
            let ep = format!("/v1/connectors/{}/connection", seg(&c.id));
            let r = self
                .api
                .put_as(
                    key.as_deref(),
                    &ep,
                    &json!({ "accessToken": access, "refreshToken": refresh }),
                )
                .map_err(conn)?;
            if r.ok() {
                self.ctx.conn.rest.push((c.id.clone(), key.clone()));
            }
            return self.judge_with(&format!("PUT {ep}"), c.refused, c.contains.as_deref(), &r);
        }
        if c.via == Some(Via::Tokens) {
            return self.connect_org_tokens(
                c,
                key,
                &client_id,
                &authorize,
                &token_endpoint,
                &mcp_url,
            );
        }
        let redirect = c
            .redirect_uri
            .clone()
            .unwrap_or_else(|| APP_REDIRECT_URI.into());
        let verifier = format!("{}{}", rand_token(), rand_token());
        let challenge = sha256_b64url(&verifier);
        let path = authorize
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|i| rest[i..].to_string()))
            .unwrap_or_else(|| "/authorize".to_string());
        let pct = |s: &str| {
            s.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        };
        let state = rand_token();
        // The runner plays the browser: it follows the pinned endpoint's path on the mock.
        let q = format!(
            "{path}?response_type=code&client_id={}&redirect_uri={}&code_challenge={challenge}&code_challenge_method=S256&state={state}",
            pct(&client_id),
            pct(&redirect)
        );
        // The runner plays the browser at the provider's listener: a company's own for ticketdesk.
        let browser_at = if ticketdesk {
            let host = authorize
                .split_once("://")
                .map(|x| x.1)
                .and_then(|r| r.split('/').next())
                .unwrap_or_default();
            mocks.ticketdesk_addr(host.split('.').next().unwrap_or_default())
        } else {
            mocks.filebox_addr
        };
        let code = browser_get_code(browser_at, &q).map_err(|e| {
            Fail::new(
                Class::Harness,
                "the filebox authorize endpoint gives a code",
                e,
            )
        })?;
        // Codes and verifiers are single-use, but they are still kept out of every output.
        self.remember(&c.id, &code);
        self.remember(&c.id, &verifier);
        let ep = format!("/v1/connectors/{}/connection/oauth/code", seg(&c.id));
        let r = self
            .api
            .post_as(
                key.as_deref(),
                &ep,
                &json!({ "code": code, "codeVerifier": verifier, "redirectUri": redirect }),
            )
            .map_err(conn)?;
        if r.ok() {
            if ticketdesk {
                self.ctx.conn.rest.push((c.id.clone(), key.clone()));
            } else {
                self.ctx.conn.mcp.push((c.id.clone(), key.clone()));
            }
        }
        self.sync_filebox();
        self.judge_with(&format!("POST {ep}"), c.refused, c.contains.as_deref(), &r)
    }

    /// An app that holds the tokens itself: the routes a connector with `org_vault` must refuse.
    fn connect_org_tokens(
        &mut self,
        c: &ConnectorConnect,
        key: Option<String>,
        client_id: &str,
        authorize: &str,
        token_endpoint: &str,
        mcp_url: &str,
    ) -> Result<(), Fail> {
        let origin = authorize
            .rsplit_once('/')
            .map(|x| x.0)
            .unwrap_or(authorize)
            .to_string();
        let body = json!({
            "connectionId": c.id,
            "url": mcp_url,
            "authMode": "oauth",
            "oauth": {
                "resource": mcp_url,
                "authorizationServer": origin,
                "tokenEndpoint": c.token_endpoint.clone().unwrap_or_else(|| token_endpoint.to_string()),
                "clientId": c.client_id.clone().unwrap_or_else(|| client_id.to_string()),
                "registrationMethod": "manual",
                "scopes": []
            }
        });
        let r = self
            .api
            .put_as(key.as_deref(), "/v1/mcp/connections", &body)
            .map_err(conn)?;
        if r.ok() {
            self.ctx.conn.mcp.push((c.id.clone(), key.clone()));
        } else {
            return self.judge_with(
                "PUT /v1/mcp/connections",
                c.refused,
                c.contains.as_deref(),
                &r,
            );
        }
        let (access, refresh) = (rand_token(), rand_token());
        self.remember(&c.id, &access);
        self.remember(&c.id, &refresh);
        let ep = format!("/v1/mcp/connections/{}/oauth/tokens", seg(&c.id));
        let r = self
            .api
            .put_as(
                key.as_deref(),
                &ep,
                &json!({ "accessToken": access, "refreshToken": refresh }),
            )
            .map_err(conn)?;
        self.judge_with(&format!("PUT {ep}"), c.refused, c.contains.as_deref(), &r)
    }

    pub(super) fn api_call(&mut self, a: &ApiCallStep) -> Result<(), Fail> {
        let key = self.key_of(a.as_.as_deref())?;
        let r = match a.method.to_ascii_uppercase().as_str() {
            "GET" => self.api.get_as(key.as_deref(), &a.path),
            "DELETE" => self.api.delete_as(key.as_deref(), &a.path),
            other => {
                return Err(Fail::new(
                    Class::Scenario,
                    "`api-call` method GET or DELETE",
                    other.to_string(),
                ))
            }
        }
        .map_err(conn)?;
        let step = format!("{} {}", a.method.to_ascii_uppercase(), a.path);
        self.judge_with(&step, a.refused, a.contains.as_deref(), &r)?;
        if let Some(alias) = &a.absent {
            self.sync_filebox();
            let values: Vec<String> = self
                .ctx
                .secrets
                .iter()
                .filter(|(k, _)| *k == alias || k.starts_with(&format!("{alias}#")))
                .map(|(_, v)| v.0.clone())
                .collect();
            if values.is_empty() {
                return Err(Fail::new(
                    Class::Scenario,
                    format!("`{alias}` holds secrets"),
                    "not a filebox mock-provider piece in `given`",
                ));
            }
            for v in values {
                if let Some(form) = crate::forms::find_form(&v, &r.body) {
                    return Err(product(
                        format!("no form of `{alias}`'s secrets in the answer to {step}"),
                        format!("the {form} form is in the body"),
                    ));
                }
            }
        }
        Ok(())
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
        if matches!(kind, Some(Kind::Filebox | Kind::Ticketdesk)) {
            return self.connect_org_client(c, key, kind == Some(Kind::Ticketdesk));
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
        self.judge_with(
            "PUT /v1/mcp/connections",
            c.refused,
            c.contains.as_deref(),
            &r,
        )?;
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
            let mut st = mocks.login.lock().unwrap_or_else(|e| e.into_inner());
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
        self.judge_with(&format!("PUT {path}"), c.refused, c.contains.as_deref(), &r)
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
        self.sync_filebox();
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
        // Exact values, `{ present: true }` and `{ contains: text }`; `absent` paths must not exist.
        self.mkt_fields(
            &format!("connector `{}`", x.id),
            entry,
            &x.fields,
            &x.absent,
        )
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
            .filter(|l| x.with.as_ref().is_none_or(|w| l.fields.contains(w)))
            .cloned()
            .collect();
        let all: BTreeSet<&String> = seen.iter().flat_map(|l| l.fields.iter()).collect();
        let want = format!("{} saw requests at {}", x.piece, x.path);
        let none_wanted = x.count == Some(0);
        let short = x.at_least.is_some_and(|n| seen.len() < n);
        if (seen.is_empty() && !none_wanted) || short || x.count.is_some_and(|n| n != seen.len()) {
            return Err(product(
                format!(
                    "{want}{}{}{}",
                    x.with
                        .as_ref()
                        .map(|w| format!(" with `{w}`"))
                        .unwrap_or_default(),
                    x.count.map(|n| format!(" ({n})")).unwrap_or_default(),
                    x.at_least
                        .map(|n| format!(" (at least {n})"))
                        .unwrap_or_default()
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

    #[test]
    fn cleanup_clears_a_company_address_with_or_without_a_client() {
        // An address saved without a client is still cleared.
        let b = node_reset_body(false, true);
        assert!(b.get("tenant").is_some_and(Value::is_null), "{b}");
        assert!(b.get("client").is_none(), "{b}");
        let b = node_reset_body(true, true);
        assert!(b["tenant"].is_null() && b.get("client").is_some_and(Value::is_null), "{b}");
        let b = node_reset_body(true, false);
        assert!(b.get("tenant").is_none(), "{b}");
        assert_eq!(node_reset_body(false, false), json!({ "enabled": false, "consent": null }));
    }

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

    fn filebox_ready(m: &Mocks) {
        let mut st = m.filebox.lock().unwrap();
        *st = State::default();
        st.fb.secret = "s-current".into();
        st.fb.wrong = "s-wrong".into();
    }

    fn filebox_code(m: &Mocks, verifier: &str) -> String {
        let q = format!(
            "/authorize?response_type=code&client_id=e2e-filebox-client&redirect_uri=squrell%3A%2F%2Foauth&code_challenge={}&code_challenge_method=S256&state=x",
            sha256_b64url(verifier)
        );
        browser_get_code(m.filebox_addr, &q).unwrap()
    }

    fn token(m: &Mocks, body: &str) -> String {
        post(m.filebox_addr, "/token", "", body)
    }

    #[test]
    fn filebox_exchanges_a_code_once_with_the_secret_and_the_verifier() {
        let m = Mocks::start().unwrap();
        filebox_ready(&m);
        let code = filebox_code(&m, "v-verifier");
        let form = |secret: &str, verifier: &str, code: &str| {
            format!(
                "grant_type=authorization_code&client_id=e2e-filebox-client&client_secret={secret}&code={code}&code_verifier={verifier}&redirect_uri=squrell%3A%2F%2Foauth"
            )
        };
        assert!(token(&m, &form("s-wrong", "v-verifier", &code)).contains("invalid_client"));
        // The wrong secret did not spend the code.
        assert!(token(&m, &form("s-current", "other", &code)).contains("invalid_grant"));
        // A wrong verifier spends it: codes are single-use.
        assert!(token(&m, &form("s-current", "v-verifier", &code)).contains("invalid_grant"));
        let code = filebox_code(&m, "v-verifier");
        let ok = token(&m, &form("s-current", "v-verifier", &code));
        assert!(
            ok.starts_with("HTTP/1.1 200") && ok.contains("refresh_token"),
            "{ok}"
        );
        let st = m.filebox.lock().unwrap();
        let f = &st
            .log
            .iter()
            .rev()
            .find(|l| l.path == "/token")
            .unwrap()
            .fields;
        assert!(f.contains("client-secret=current") && f.contains("code_verifier"));
        assert!(f.contains("redirect_uri=squrell://oauth"));
        assert!(!f
            .iter()
            .any(|x| x.contains("s-current") || x.contains("v-verifier")));
        assert_eq!(st.fb.issued.len(), 2);
    }

    #[test]
    fn filebox_refresh_tokens_are_single_use_and_the_secret_can_rotate() {
        let m = Mocks::start().unwrap();
        filebox_ready(&m);
        let code = filebox_code(&m, "v");
        let first = token(
            &m,
            &format!("grant_type=authorization_code&client_id=e2e-filebox-client&client_secret=s-current&code={code}&code_verifier=v&redirect_uri=squrell%3A%2F%2Foauth"),
        );
        let json: Value = serde_json::from_str(first.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let (access, refresh) = (
            json["access_token"].as_str().unwrap().to_string(),
            json["refresh_token"].as_str().unwrap().to_string(),
        );
        let rpc =
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_files"}}"#;
        let call = |bearer: &str| {
            post(
                m.filebox_addr,
                "/mcp",
                &format!("Authorization: Bearer {bearer}\r\n"),
                rpc,
            )
        };
        assert!(call(&access).starts_with("HTTP/1.1 200"));
        // The access token expires: refused and logged as expired, not wrong.
        {
            let mut st = m.filebox.lock().unwrap();
            let cur = std::mem::take(&mut st.fb.access);
            st.fb.stale.insert(cur);
        }
        assert!(call(&access).starts_with("HTTP/1.1 401"));
        assert!(call("nonsense").starts_with("HTTP/1.1 401"));
        let refresh_form = |secret: &str, rt: &str| {
            format!("grant_type=refresh_token&client_id=e2e-filebox-client&client_secret={secret}&refresh_token={rt}")
        };
        let ok = token(&m, &refresh_form("s-current", &refresh));
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        // Spent once: the same token again is `invalid_grant`.
        assert!(token(&m, &refresh_form("s-current", &refresh)).contains("invalid_grant"));
        // The provider rotates its secret: the old one is `old`, refused as a client.
        {
            let mut st = m.filebox.lock().unwrap();
            let old = std::mem::replace(&mut st.fb.secret, "s-new".into());
            st.fb.old.insert(old);
        }
        assert!(token(&m, &refresh_form("s-current", &refresh)).contains("invalid_client"));
        let st = m.filebox.lock().unwrap();
        let fields: Vec<&BTreeSet<String>> = st
            .log
            .iter()
            .filter(|l| l.path == "/token")
            .map(|l| &l.fields)
            .collect();
        assert!(fields.iter().any(|f| f.contains("refresh=fresh")));
        assert!(fields.iter().any(|f| f.contains("refresh=reused")));
        assert!(fields.iter().any(|f| f.contains("client-secret=old")));
        let mcp: Vec<&BTreeSet<String>> = st
            .log
            .iter()
            .filter(|l| l.path == "/mcp")
            .map(|l| &l.fields)
            .collect();
        assert!(mcp.iter().any(|f| f.contains("authorization-expired")));
        assert!(mcp.iter().any(|f| f.contains("authorization-wrong")));
        assert!(mcp.iter().any(|f| f.contains("tool-ok=search_files")));
    }

    #[test]
    fn filebox_holds_a_refresh_until_released() {
        let m = Mocks::start().unwrap();
        filebox_ready(&m);
        {
            let mut st = m.filebox.lock().unwrap();
            st.fb.live.insert("r1".into());
            st.fb.hold_token = true;
        }
        let addr = m.filebox_addr;
        let t = std::thread::spawn(move || {
            post(
                addr,
                "/token",
                "",
                "grant_type=refresh_token&client_id=e2e-filebox-client&client_secret=s-current&refresh_token=r1",
            )
        });
        // Wait (bounded) for the request to arrive and be logged, then it must still be held.
        let waited = std::time::Instant::now();
        while m.filebox.lock().unwrap().log.is_empty()
            && waited.elapsed() < std::time::Duration::from_secs(10)
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(m.filebox.lock().unwrap().log.len(), 1, "logged on arrival");
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!t.is_finished(), "held while the hold is on");
        m.filebox.lock().unwrap().fb.hold_token = false;
        assert!(t.join().unwrap().starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn a_revoked_filebox_refuses_a_refresh_before_consuming_the_token() {
        let m = Mocks::start().unwrap();
        filebox_ready(&m);
        {
            let mut st = m.filebox.lock().unwrap();
            st.fb.live.insert("r1".into());
            st.revoked = true;
        }
        let body = "grant_type=refresh_token&client_id=e2e-filebox-client&client_secret=s-current&refresh_token=r1";
        let answer = token(&m, body);
        assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
        assert!(answer.contains("invalid_grant"), "{answer}");
        {
            let st = m.filebox.lock().unwrap();
            assert!(st.fb.live.contains("r1"), "the token was not consumed");
            assert!(st.log.last().unwrap().fields.contains("refresh=revoked"));
        }
        // Not revoked: the single-use behavior is as before.
        m.filebox.lock().unwrap().revoked = false;
        assert!(token(&m, body).starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn a_secret_rotated_while_a_refresh_is_held_is_refused_when_it_is_released() {
        let m = Mocks::start().unwrap();
        filebox_ready(&m);
        {
            let mut st = m.filebox.lock().unwrap();
            st.fb.live.insert("r1".into());
            st.fb.hold_token = true;
        }
        let addr = m.filebox_addr;
        let t = std::thread::spawn(move || {
            post(
                addr,
                "/token",
                "",
                "grant_type=refresh_token&client_id=e2e-filebox-client&client_secret=s-current&refresh_token=r1",
            )
        });
        let waited = std::time::Instant::now();
        while m.filebox.lock().unwrap().log.is_empty()
            && waited.elapsed() < std::time::Duration::from_secs(10)
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // The provider rotates its secret while the request is held: the old one is refused.
        {
            let mut st = m.filebox.lock().unwrap();
            let old = std::mem::replace(&mut st.fb.secret, "s-rotated".into());
            st.fb.old.insert(old);
            st.fb.hold_token = false;
        }
        let answer = t.join().unwrap();
        assert!(answer.starts_with("HTTP/1.1 401"), "{answer}");
        // Logged as it arrived (current), refused on what is current now, token not spent.
        let st = m.filebox.lock().unwrap();
        assert!(st.log[0].fields.contains("client-secret=current"));
        assert!(st.fb.live.contains("r1"));
    }

    #[test]
    fn filebox_serves_metadata_without_a_registration_endpoint() {
        let m = Mocks::start().unwrap();
        let get = |path: &str| {
            let mut s = TcpStream::connect(m.filebox_addr).unwrap();
            write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        };
        let prm = get("/.well-known/oauth-protected-resource/mcp");
        assert!(prm.contains("https://auth.filebox.example") && prm.contains(FILEBOX_MCP_URL));
        let asm = get("/.well-known/oauth-authorization-server");
        assert!(asm.contains("code_challenge_methods_supported") && asm.contains("S256"));
        assert!(!asm.contains("registration_endpoint"));
    }

    fn get(addr: SocketAddr, path: &str, extra: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n{extra}\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn ticketdesk_is_a_public_client_per_company_with_rotating_refresh_tokens() {
        let m = Mocks::start().unwrap();
        *m.ticketdesk.lock().unwrap() = State::default();
        let (acme, globex) = (m.ticketdesk_addr("acme"), m.ticketdesk_addr("globex"));
        assert_ne!(acme, globex);
        let q = format!(
            "/oauth/authorizations/new?response_type=code&client_id=e2e-ticketdesk-client&redirect_uri=squrell%3A%2F%2Foauth&code_challenge={}&code_challenge_method=S256&state=x",
            sha256_b64url("v")
        );
        let form = |code: &str, extra: &str| {
            format!("grant_type=authorization_code&client_id=e2e-ticketdesk-client&code={code}&code_verifier=v&redirect_uri=squrell%3A%2F%2Foauth{extra}")
        };
        // A code is bound to the company that minted it.
        let code = browser_get_code(acme, &q).unwrap();
        assert!(post(globex, "/oauth/tokens", "", &form(&code, "")).contains("invalid_grant"));
        let code = browser_get_code(acme, &q).unwrap();
        // A public client: a secret is refused, and does not spend the code.
        assert!(
            post(acme, "/oauth/tokens", "", &form(&code, "&client_secret=x"))
                .contains("invalid_client")
        );
        let ok = post(acme, "/oauth/tokens", "", &form(&code, ""));
        assert!(ok.contains("refresh_token"), "{ok}");
        let (access, refresh) = {
            let st = m.ticketdesk.lock().unwrap();
            (st.td.issued[0].clone(), st.td.issued[1].clone())
        };
        let auth = format!("Authorization: Bearer {access}\r\n");
        assert!(get(acme, "/api/v2/search.json?query=a", &auth).contains("Printer on fire"));
        // Another company's instance does not accept this company's token.
        assert!(get(globex, "/api/v2/search.json", &auth).starts_with("HTTP/1.1 401"));
        assert!(get(
            acme,
            "/api/v2/search.json",
            "Authorization: vamsec_ticketdesk_api\r\n"
        )
        .starts_with("HTTP/1.1 401"));
        // Rotation: the refresh token is spent once and the old access token goes stale.
        let r = format!(
            "grant_type=refresh_token&client_id=e2e-ticketdesk-client&refresh_token={refresh}"
        );
        assert!(post(acme, "/oauth/tokens", "", &r).contains("refresh_token"));
        assert!(post(acme, "/oauth/tokens", "", &r).contains("invalid_grant"));
        assert!(get(acme, "/api/v2/search.json", &auth).starts_with("HTTP/1.1 401"));
        let st = m.ticketdesk.lock().unwrap();
        let fields = |bearer: &str| {
            st.log.iter().any(|l| {
                l.fields.contains(&format!("bearer={bearer}")) && l.fields.contains("host=acme")
            })
        };
        assert!(fields("current") && fields("placeholder") && fields("stale"));
        assert!(st.log.iter().any(|l| l.fields.contains("refresh=reused")));
        assert!(st
            .log
            .iter()
            .any(|l| l.fields.contains("client-secret=present")));
        assert!(!st
            .log
            .iter()
            .flat_map(|l| l.fields.iter())
            .any(|f| f.contains(&access) || f.contains(&refresh)));
    }

    #[test]
    fn ticketdesk_logs_a_request_it_does_not_serve() {
        let m = Mocks::start().unwrap();
        *m.ticketdesk.lock().unwrap() = State::default();
        let acme = m.ticketdesk_addr("acme");
        let r = get(acme, "/.well-known/oauth-authorization-server", "");
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
        let st = m.ticketdesk.lock().unwrap();
        assert!(
            st.log
                .iter()
                .any(|l| l.path == "/.well-known/oauth-authorization-server"
                    && l.fields.contains("host=acme")),
            "a discovery probe is on the record"
        );
    }

    #[test]
    fn form_decode_handles_plus_and_percent() {
        assert_eq!(form_decode("a+b%2Fc%"), "a b/c%");
    }
}
