//! The per-call egress envelope for connection bindings (CN-2.2, CN-2.3).
//!
//! A connector binding (`vamsec_m365_graph`) is a placeholder the guest sends and the host swaps
//! for the member's access token. The swap is allowed only inside the shape the calling tool
//! declared in its manifest: binding, exact host, method, and either a path prefix on a segment
//! boundary (reads) or an exact path (writes, CN-4.3: `/v1.0/me/events` does not admit
//! `/v1.0/me/events/x/forward`). Everything here is pure: no DNS, no vault, no token.
//! [`Envelope::screen`] decides before any secret work; the token source reads the token only
//! after the destination check and only for a [`ConnectionGrant`] it was handed.
//!
//! Path rules (open question 3): one percent-decode per segment, dot segments and `//` refused,
//! query and fragment ignored, case-sensitive, host exact (no port other than 443, no
//! subdomain, no trailing dot), https only, no userinfo. Both kinds of path go through the same
//! [`normalize_path`]; an exact path also refuses a trailing slash on the request.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use vam_spec::v1::EgressResponse;

/// One shape a tool declared: what it may reach through a connector binding.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Shape {
    pub binding: String,
    pub host: String,
    pub methods: Vec<String>,
    pub path_prefixes: Vec<String>,
    /// Exact paths: the request path must equal one of these (no trailing slash, nothing
    /// beneath). Rows stored before this field existed read as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// A binding a connector definition offers: the placeholder the guest holds and the hosts the
/// definition pins it to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingSpec {
    /// The connector URN a skill names in `requiresConnector`.
    pub connector: String,
    pub name: String,
    pub placeholder: String,
    pub hosts: Vec<String>,
}

/// The host's record of the tool call that is running now.
#[derive(Debug, Clone)]
pub struct ActiveCall {
    pub call_id: String,
    pub tool: String,
    pub requires_connector: Option<String>,
    pub shapes: Vec<Shape>,
    /// The lifecycle generation of the lease the call was authorized under (the effect journal
    /// records the generation a write left from).
    pub generation: u64,
    deadline: Instant,
}

impl ActiveCall {
    pub fn new(
        call_id: impl Into<String>,
        tool: impl Into<String>,
        requires_connector: Option<String>,
        shapes: Vec<Shape>,
        ttl: Duration,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool: tool.into(),
            requires_connector,
            shapes,
            generation: 0,
            deadline: Instant::now() + ttl,
        }
    }

    /// Records the lease generation the call was authorized under.
    pub fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }
}

/// One per session: the call the PEP last authorized (open question 1). Replaced by the next
/// intent, cleared at turn commit, at the next inference request, at session end, or when the
/// tool timeout plus a grace has passed.
#[derive(Debug, Default)]
pub struct CallWindow {
    slot: Mutex<Option<ActiveCall>>,
}

impl CallWindow {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self, call: ActiveCall) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(call);
    }

    pub fn clear(&self) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The active call, if its deadline has not passed (an expired one is dropped).
    pub fn current(&self) -> Option<ActiveCall> {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|c| Instant::now() >= c.deadline) {
            *slot = None;
        }
        slot.clone()
    }
}

/// What the gate shows the envelope: the effective request after URL-site substitution.
pub struct ConnectionRequest<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a HashMap<String, String>,
    pub body: &'a [u8],
}

/// A request the envelope admitted. `attach` must find the same call still active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionGrant {
    pub call_id: String,
    pub tool: String,
    pub connector: String,
    pub binding: String,
    pub host: String,
    /// The request's method, upper case.
    pub method: String,
    /// The lease generation of the active call.
    pub generation: u64,
}

/// A request method that cannot change anything at the far end. Every other method is a write
/// (the effect journal records it before it is sent).
pub fn is_read_method(method: &str) -> bool {
    method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD")
}

/// A refusal: fixed text for the guest, and the rule for the `egress.decided` DENY event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionRefusal {
    pub text: String,
    pub rule: String,
}

impl std::fmt::Display for ConnectionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Where a connection token is attached. Implemented by the orchestrator.
#[async_trait]
pub trait ConnectionTokenSource: Send + Sync {
    /// Pure screen, before DNS and before any secret is touched. `Ok(None)` means the request
    /// carries no connection placeholder and takes the static path.
    fn authorize(
        &self,
        request: &ConnectionRequest<'_>,
    ) -> Result<Option<ConnectionGrant>, ConnectionRefusal>;

    /// Called only after the destination was allowed. Re-checks the grant against the window,
    /// reads the token and puts it in the request headers.
    async fn attach(
        &self,
        grant: &ConnectionGrant,
        headers: &mut HashMap<String, String>,
    ) -> Result<(), ConnectionRefusal>;

    /// Removes the tokens this source attached from a response.
    fn scrub(&self, response: &mut EgressResponse);

    /// Called after `authorize` and before `attach`, for every admitted request. A source that
    /// journals writes returns a ticket for one (and refuses when the journal cannot be
    /// written: nothing is attached and nothing is sent); a read, or a source that journals
    /// nothing, returns `None`. The caller must call [`WriteTicket::commit_send`] immediately
    /// before the request leaves the host and send only if it returns `Ok`.
    async fn reserve_write(
        &self,
        _grant: &ConnectionGrant,
    ) -> Result<Option<Box<dyn WriteTicket>>, ConnectionRefusal> {
        Ok(None)
    }

    /// True when `host` is a destination this source can attach a token to, whether or not a
    /// given request carries a placeholder. The gate marks such a host secret-bound
    /// (`PreparedEgress::secret_bound`): a response from it that the proxy could not scrub is
    /// refused, never returned. Required, so an implementation cannot forget it.
    fn binds_host(&self, host: &str) -> bool;
}

/// A journaled write the token source reserved at the envelope (CN-4.7). The ordering the
/// gate keeps: the intent was written before the call was authorized; `commit_send` writes
/// `sent` immediately before the request leaves the host; `answered` records the far end's
/// answer. A ticket that is dropped after `commit_send` without an answer leaves the call
/// `unknown` (the request may have been received); dropped before it, nothing was sent and
/// the journal is unchanged.
pub trait WriteTicket: Send {
    /// Persists that the request is about to leave. An `Err` means it must not be sent.
    fn commit_send(&mut self) -> Result<(), ConnectionRefusal>;

    /// The far end answered with this HTTP status.
    fn answered(&mut self, status: u16);
}

/// The pure half of a token source: the binding table, the window, and every shape check.
#[derive(Debug)]
pub struct Envelope {
    bindings: Vec<BindingSpec>,
    window: std::sync::Arc<CallWindow>,
}

const HOST_PORT: &str = "443";

impl Envelope {
    pub fn new(bindings: Vec<BindingSpec>, window: std::sync::Arc<CallWindow>) -> Self {
        Self { bindings, window }
    }

    pub fn window(&self) -> &std::sync::Arc<CallWindow> {
        &self.window
    }

    /// True when `host` is pinned by any binding that has a placeholder (case-insensitive,
    /// trailing dot ignored; hosts are matched exactly, as `screen` does).
    pub fn binds_host(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        self.bindings.iter().any(|b| {
            !b.placeholder.is_empty() && b.hosts.iter().any(|h| h.eq_ignore_ascii_case(&host))
        })
    }

    pub fn binding(&self, connector: &str, name: &str) -> Option<&BindingSpec> {
        self.bindings
            .iter()
            .find(|b| b.connector == connector && b.name == name)
    }

    /// See the module docs. Order: placeholder site, target form, window, tool declaration,
    /// binding and connector, host, method, path.
    pub fn screen(
        &self,
        req: &ConnectionRequest<'_>,
    ) -> Result<Option<ConnectionGrant>, ConnectionRefusal> {
        let present: Vec<&BindingSpec> = self
            .bindings
            .iter()
            .filter(|b| !b.placeholder.is_empty() && carries(req, &b.placeholder))
            .collect();
        let spec = match present.as_slice() {
            [] => return Ok(None),
            [one] => *one,
            _ => return Err(self.refuse_form(None, "more than one connection placeholder")),
        };

        let window = self.window.current();
        let tool = window.as_ref().map(|c| c.tool.as_str());

        if !placeholder_only_in_authorization(req, &spec.placeholder) {
            return Err(self.refuse_form(
                tool,
                "the placeholder is only valid as the Authorization value",
            ));
        }
        let target = match Target::parse(req.url) {
            Ok(t) => t,
            Err(why) => return Err(self.refuse_form(tool, why)),
        };

        let Some(call) = window else {
            return Err(ConnectionRefusal {
                text: "Connection credential refused: no tool call is active, so this request is outside any declared shape.".into(),
                rule: "connection egress (no active tool call)".into(),
            });
        };
        let rule = format!("tool:{} egress", call.tool);
        let outside = || ConnectionRefusal {
            text: outside_text(&call),
            rule: rule.clone(),
        };
        if call.shapes.is_empty() || call.requires_connector.is_none() {
            return Err(ConnectionRefusal {
                text: fit(format!(
                    "Connection credential refused: tool '{}' declares no egress shape, so this request is outside its declared shape.",
                    call.tool
                )),
                rule: rule.clone(),
            });
        }
        if call.requires_connector.as_deref() != Some(spec.connector.as_str()) {
            return Err(outside());
        }
        let method = req.method.to_ascii_uppercase();
        let segments = match normalize_path(&target.path) {
            Ok(s) => s,
            Err(_) => return Err(outside()),
        };
        let host_pinned = spec.hosts.contains(&target.host);
        let admitted = host_pinned
            && call.shapes.iter().any(|s| {
                s.binding == spec.name
                    && s.host == target.host
                    && s.methods.contains(&method)
                    && (s.path_prefixes.iter().any(|p| prefix_admits(p, &segments))
                        || s.paths
                            .iter()
                            .any(|p| exact_admits(p, &target.path, &segments)))
            });
        if !admitted {
            return Err(outside());
        }
        Ok(Some(ConnectionGrant {
            call_id: call.call_id,
            tool: call.tool,
            connector: spec.connector.clone(),
            binding: spec.name.clone(),
            host: target.host,
            method,
            generation: call.generation,
        }))
    }

    /// No gap between `screen` and `attach`: the call the grant was made for must still be the
    /// active one.
    pub fn recheck(&self, grant: &ConnectionGrant) -> Result<(), ConnectionRefusal> {
        match self.window.current() {
            Some(c) if c.call_id == grant.call_id && c.tool == grant.tool => Ok(()),
            _ => Err(ConnectionRefusal {
                text: "Connection credential refused: the tool call ended, so this request is outside any declared shape.".into(),
                rule: format!("tool:{} egress", grant.tool),
            }),
        }
    }

    fn refuse_form(&self, tool: Option<&str>, why: &str) -> ConnectionRefusal {
        ConnectionRefusal {
            text: fit(format!(
                "Connection credential refused: outside the declared shape ({why})."
            )),
            rule: match tool {
                Some(t) => format!("tool:{t} egress"),
                None => "connection egress".into(),
            },
        }
    }
}

fn outside_text(call: &ActiveCall) -> String {
    let declared: Vec<String> = call
        .shapes
        .iter()
        .map(|s| {
            let reach: Vec<String> = s
                .path_prefixes
                .iter()
                .cloned()
                .chain(s.paths.iter().map(|p| format!("exactly {p}")))
                .collect();
            format!("{} {} {}", s.methods.join("/"), s.host, reach.join(","))
        })
        .collect();
    fit(format!(
        "Connection credential refused: this request is outside the declared shape of tool '{}' ({}).",
        call.tool,
        declared.join("; ")
    ))
}

/// Keeps a refusal on one short line (the port-55 reply cuts at 200 characters).
fn fit(mut s: String) -> String {
    s.retain(|c| !c.is_control());
    if s.len() > 190 {
        let mut end = 189;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}

fn carries(req: &ConnectionRequest<'_>, placeholder: &str) -> bool {
    req.url.contains(placeholder)
        || req.headers.values().any(|v| v.contains(placeholder))
        || req.headers.keys().any(|k| k.contains(placeholder))
        || contains_bytes(req.body, placeholder.as_bytes())
}

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// The placeholder is in no URL, no body, no header name, and no header but `Authorization`;
/// and every `Authorization` header (any letter case) is exactly the placeholder, bare or as a
/// `Bearer` credential.
fn placeholder_only_in_authorization(req: &ConnectionRequest<'_>, placeholder: &str) -> bool {
    if req.url.contains(placeholder)
        || contains_bytes(req.body, placeholder.as_bytes())
        || req.headers.keys().any(|k| k.contains(placeholder))
    {
        return false;
    }
    let mut seen = false;
    for (name, value) in req.headers {
        if name.eq_ignore_ascii_case("authorization") {
            if !authorization_is(value, placeholder) {
                return false;
            }
            seen = true;
        } else if value.contains(placeholder) {
            return false;
        }
    }
    seen
}

fn authorization_is(value: &str, placeholder: &str) -> bool {
    let v = value.trim();
    if v == placeholder {
        return true;
    }
    match v.split_once(' ') {
        Some((scheme, rest)) => scheme.eq_ignore_ascii_case("bearer") && rest.trim() == placeholder,
        None => false,
    }
}

/// Sets the one `Authorization` header to `Bearer <token>`, dropping every other spelling.
pub fn set_bearer(headers: &mut HashMap<String, String>, token: &str) {
    headers.retain(|k, _| !k.eq_ignore_ascii_case("authorization"));
    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
}

/// Replaces `secret` in a response with `placeholder`, in headers and body (every encoded form
/// the static scrub covers).
pub fn scrub_secret(response: &mut EgressResponse, secret: &str, placeholder: &str) {
    if secret.is_empty() {
        return;
    }
    let redactor = vam_security::secret_forms::SecretRedactor::with_replacement(
        &[secret.as_bytes()],
        placeholder,
    );
    for val in response.headers.values_mut() {
        if let std::borrow::Cow::Owned(scrubbed) = redactor.redact_str(val) {
            *val = scrubbed;
        }
    }
    if let std::borrow::Cow::Owned(scrubbed) = redactor.redact_bytes(&response.body) {
        response.body = scrubbed;
    }
}

/// The parts of a request URL the envelope trusts, read from the raw string (the `url` crate
/// folds `..` and `%2e%2e` before anyone can see them).
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub path: String,
}

impl Target {
    pub fn parse(url: &str) -> Result<Self, &'static str> {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("HTTPS://"))
            .ok_or("https only")?;
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        if authority.contains('@') {
            return Err("no credentials in the URL");
        }
        let host = match authority.rsplit_once(':') {
            Some((h, port)) if port == HOST_PORT && !h.contains(':') => h,
            Some(_) => return Err("host port not allowed"),
            None => authority,
        };
        if host.is_empty() || host.ends_with('.') || host.starts_with('[') {
            return Err("host form not allowed");
        }
        let host = host.to_ascii_lowercase();
        let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
        let path = if path_end == 0 {
            "/"
        } else {
            &tail[..path_end]
        };
        // What the gate will dispatch must be what was checked.
        let parsed = reqwest::Url::parse(url).map_err(|_| "unparsable URL")?;
        if parsed.host_str() != Some(host.as_str()) || parsed.path() != path {
            return Err("URL is not in canonical form");
        }
        Ok(Self {
            host,
            path: path.to_string(),
        })
    }
}

/// Splits a request path into decoded segments, or says why it is refused. A single trailing
/// empty segment (a trailing slash) is dropped.
pub fn normalize_path(raw: &str) -> Result<Vec<String>, &'static str> {
    if !raw.starts_with('/') {
        return Err("path must start with /");
    }
    if raw
        .bytes()
        .any(|b| b <= 0x20 || b == 0x7f || b == b'\\' || b >= 0x80)
    {
        return Err("control, space, backslash or non-ASCII byte");
    }
    let mut parts: Vec<&str> = raw[1..].split('/').collect();
    if parts.len() > 1 && parts.last() == Some(&"") {
        parts.pop();
    }
    if parts == [""] {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(parts.len());
    for part in parts {
        if part.is_empty() {
            return Err("empty segment");
        }
        let decoded = percent_decode_once(part).ok_or("bad percent escape")?;
        if decoded == "." || decoded == ".." {
            return Err("dot segment");
        }
        if decoded.contains('/') || decoded.contains('\\') || decoded.contains('%') {
            return Err("encoded slash or double encoding");
        }
        if decoded.chars().any(|c| c.is_control()) {
            return Err("control character");
        }
        out.push(decoded);
    }
    Ok(out)
}

fn percent_decode_once(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b.get(i + 1..i + 3)?;
            let hi = (hex[0] as char).to_digit(16)?;
            let lo = (hex[1] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A declared prefix admits a path when its segments are the leading segments of the path
/// (`/v1.0/me/messages` admits `/v1.0/me/messages/abc`, not `/v1.0/me/messagesX`). A trailing
/// slash on the prefix is ignored; `/` admits the whole host.
pub fn prefix_admits(prefix: &str, segments: &[String]) -> bool {
    let mut p: Vec<&str> = prefix.trim_start_matches('/').split('/').collect();
    while p.last() == Some(&"") {
        p.pop();
    }
    p.len() <= segments.len() && p.iter().zip(segments).all(|(a, b)| *a == b.as_str())
}

/// A declared exact path admits a request only when the request path names the same segments and
/// does not end in a slash. `raw_path` is the request path before decoding (query already cut off):
/// [`normalize_path`] drops one trailing slash, so `/events/` would otherwise equal `/events`.
/// `segments` are the once-decoded segments from [`normalize_path`], so the dot-segment,
/// encoded-slash and double-encoding refusals have already applied. A declared path that is not in
/// the form publish accepts (root, trailing slash, empty or dot segment) admits nothing.
pub fn exact_admits(exact: &str, raw_path: &str, segments: &[String]) -> bool {
    if !exact.starts_with('/') || exact.len() < 2 || exact.ends_with('/') || raw_path.ends_with('/')
    {
        return false;
    }
    let want: Vec<&str> = exact[1..].split('/').collect();
    if want.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return false;
    }
    want.len() == segments.len() && want.iter().zip(segments).all(|(a, b)| *a == b.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const PH: &str = "vamsec_m365_graph";
    const URN: &str = "urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365";

    fn shape(methods: &[&str], prefix: &str) -> Shape {
        Shape {
            binding: "graph".into(),
            host: "graph.microsoft.com".into(),
            methods: methods.iter().map(|m| m.to_string()).collect(),
            path_prefixes: vec![prefix.into()],
            paths: vec![],
        }
    }

    fn exact_shape(methods: &[&str], path: &str) -> Shape {
        Shape {
            path_prefixes: vec![],
            paths: vec![path.into()],
            ..shape(methods, "/")
        }
    }

    fn envelope(call: Option<ActiveCall>) -> Envelope {
        let window = Arc::new(CallWindow::new());
        if let Some(c) = call {
            window.open(c);
        }
        Envelope::new(
            vec![BindingSpec {
                connector: URN.into(),
                name: "graph".into(),
                placeholder: PH.into(),
                hosts: vec!["graph.microsoft.com".into()],
            }],
            window,
        )
    }

    fn read_call() -> ActiveCall {
        ActiveCall::new(
            "c1",
            "m365_probe_search",
            Some(URN.into()),
            vec![shape(&["GET"], "/v1.0/me/messages")],
            Duration::from_secs(60),
        )
    }

    fn headers(auth: &str) -> HashMap<String, String> {
        HashMap::from([("Authorization".to_string(), auth.to_string())])
    }

    fn screen(
        e: &Envelope,
        method: &str,
        url: &str,
        h: &HashMap<String, String>,
        body: &[u8],
    ) -> Result<Option<ConnectionGrant>, ConnectionRefusal> {
        e.screen(&ConnectionRequest {
            method,
            url,
            headers: h,
            body,
        })
    }

    fn get(e: &Envelope, url: &str) -> Result<Option<ConnectionGrant>, ConnectionRefusal> {
        screen(e, "GET", url, &headers(PH), b"")
    }

    #[track_caller]
    fn refused(r: Result<Option<ConnectionGrant>, ConnectionRefusal>) -> ConnectionRefusal {
        let r = r.expect_err("must be refused");
        assert!(r.text.contains("declared shape"), "{}", r.text);
        assert!(r.text.len() <= 200, "{}", r.text.len());
        assert!(!r.text.contains(PH));
        r
    }

    #[test]
    fn the_declared_read_is_admitted() {
        let e = envelope(Some(read_call()));
        for url in [
            "https://graph.microsoft.com/v1.0/me/messages",
            "https://graph.microsoft.com/v1.0/me/messages?$top=5",
            "https://graph.microsoft.com/v1.0/me/messages#frag",
            "https://graph.microsoft.com/v1.0/me/messages/",
            "https://graph.microsoft.com/v1.0/me/messages/AAMk%3D",
            "https://graph.microsoft.com/v1.0/me/%6Dessages",
            "https://graph.microsoft.com:443/v1.0/me/messages",
            "https://GRAPH.microsoft.com/v1.0/me/messages",
        ] {
            let g = get(&e, url).unwrap_or_else(|r| panic!("{url}: {}", r.text));
            let g = g.expect("grant");
            assert_eq!(g.binding, "graph");
            assert_eq!(g.host, "graph.microsoft.com");
            assert_eq!(g.tool, "m365_probe_search");
        }
        // Bearer form and lowercase header name are the same placeholder.
        let h = HashMap::from([("authorization".to_string(), format!("bearer {PH}"))]);
        assert!(screen(
            &e,
            "get",
            "https://graph.microsoft.com/v1.0/me/messages",
            &h,
            b""
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn requests_without_a_connection_placeholder_take_the_static_path() {
        let e = envelope(None);
        let h = headers("Bearer vamsec_other");
        assert_eq!(
            screen(&e, "GET", "https://example.com/x", &h, b"").unwrap(),
            None
        );
    }

    #[test]
    fn path_tricks_are_refused() {
        let e = envelope(Some(read_call()));
        for path in [
            "/v1.0/me/messagesX",
            "/v1.0/me/messages/../sendMail",
            "/v1.0/me/messages/%2e%2e/sendMail",
            "/v1.0/me/messages/%2E%2E/sendMail",
            "/v1.0/me/messages/%2e/x",
            "/v1.0/me/messages/./x",
            "/v1.0/me//messages",
            "/v1.0/me/messages//x",
            "/v1.0/me/messages%2FsendMail",
            "/v1.0/me/messages/%2Fx",
            "/v1.0/me/messages/%252e%252e/x",
            "/v1.0/me/messages/%5cx",
            "/v1.0/me/messages/x%00",
            "/v1.0/me/messages/%zz",
            "/V1.0/me/messages",
            "/v1.0/me/messages;a=1",
            "/v1.0/users",
            "/v1.0/me",
            "/",
            "//graph.microsoft.com/v1.0/me/messages",
        ] {
            let url = format!("https://graph.microsoft.com{path}");
            refused(get(&e, &url));
        }
        refused(get(&e, "https://graph.microsoft.com/v1.0/me/messages/a b"));
        refused(get(&e, "https://graph.microsoft.com/v1.0/me/messages/a\tb"));
        refused(get(&e, "https://graph.microsoft.com/v1.0/me/messages/a\\b"));
        refused(get(&e, "https://graph.microsoft.com/v1.0/me/messages/é"));
    }

    #[test]
    fn host_and_scheme_must_be_exact() {
        let e = envelope(Some(read_call()));
        for url in [
            "https://graph.microsoft.com:8443/v1.0/me/messages",
            "https://graph.microsoft.com./v1.0/me/messages",
            "https://sub.graph.microsoft.com/v1.0/me/messages",
            "https://graph.microsoft.com.evil.com/v1.0/me/messages",
            "https://user@graph.microsoft.com/v1.0/me/messages",
            "https://graph.microsoft.com@evil.com/v1.0/me/messages",
            "https://evil.com/v1.0/me/messages",
            "http://graph.microsoft.com/v1.0/me/messages",
            "https://127.0.0.1/v1.0/me/messages",
            "https://[::1]/v1.0/me/messages",
            "graph.microsoft.com/v1.0/me/messages",
        ] {
            refused(get(&e, url));
        }
    }

    #[test]
    fn method_must_be_declared() {
        let e = envelope(Some(read_call()));
        let url = "https://graph.microsoft.com/v1.0/me/messages";
        for m in [
            "POST", "post", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "TRACE", "CONNECT",
        ] {
            refused(screen(&e, m, url, &headers(PH), b""));
        }
    }

    #[test]
    fn a_read_tool_cannot_reach_send_mail() {
        let e = envelope(Some(read_call()));
        refused(screen(
            &e,
            "POST",
            "https://graph.microsoft.com/v1.0/me/sendMail",
            &headers(PH),
            b"{}",
        ));
        refused(get(&e, "https://graph.microsoft.com/v1.0/users"));
    }

    #[test]
    fn a_send_tool_window_admits_its_post_only() {
        let call = ActiveCall::new(
            "c2",
            "m365_probe_send",
            Some(URN.into()),
            vec![shape(&["POST"], "/v1.0/me/sendMail")],
            Duration::from_secs(60),
        );
        let e = envelope(Some(call));
        let url = "https://graph.microsoft.com/v1.0/me/sendMail";
        assert!(screen(&e, "POST", url, &headers(PH), b"{}")
            .unwrap()
            .is_some());
        refused(screen(&e, "GET", url, &headers(PH), b""));
        refused(get(&e, "https://graph.microsoft.com/v1.0/me/messages"));
    }

    fn exact_call(method: &str, path: &str) -> Envelope {
        envelope(Some(ActiveCall::new(
            "c9",
            "m365_create_event",
            Some(URN.into()),
            vec![exact_shape(&[method], path)],
            Duration::from_secs(60),
        )))
    }

    #[test]
    fn an_exact_path_admits_that_path_only() {
        let e = exact_call("POST", "/v1.0/me/events");
        let post = |url: &str| screen(&e, "POST", url, &headers(PH), b"{}");
        for url in [
            "https://graph.microsoft.com/v1.0/me/events",
            "https://graph.microsoft.com/v1.0/me/events?x=1",
            "https://graph.microsoft.com/v1.0/me/events#frag",
            "https://graph.microsoft.com:443/v1.0/me/events",
            "https://graph.microsoft.com/v1.0/me/%65vents",
        ] {
            let g = post(url).unwrap_or_else(|r| panic!("{url}: {}", r.text));
            assert_eq!(g.expect("grant").tool, "m365_create_event");
        }
        // Everything the create could otherwise reach by prefix.
        for url in [
            "https://graph.microsoft.com/v1.0/me/events/x/forward",
            "https://graph.microsoft.com/v1.0/me/events/x/cancel",
            "https://graph.microsoft.com/v1.0/me/events/x/decline",
            "https://graph.microsoft.com/v1.0/me/events/x",
            "https://graph.microsoft.com/v1.0/me/events/",
            "https://graph.microsoft.com/v1.0/me/events/?x=1",
            "https://graph.microsoft.com/v1.0/me/events//",
            "https://graph.microsoft.com/v1.0/me/events%2F",
            "https://graph.microsoft.com/v1.0/me/events%2Fx%2Fforward",
            "https://graph.microsoft.com/v1.0/me/events/%2E%2E/sendMail",
            "https://graph.microsoft.com/v1.0/me/events/../sendMail",
            "https://graph.microsoft.com/v1.0/me/events/./",
            "https://graph.microsoft.com/v1.0/me/%2565vents",
            "https://graph.microsoft.com/v1.0/me/eventsX",
            "https://graph.microsoft.com/v1.0/me/Events",
            "https://graph.microsoft.com/v1.0/me",
            "https://graph.microsoft.com/v1.0/me/sendMail",
            "https://graph.microsoft.com/v1.0/me/messages/x/send",
            "https://graph.microsoft.com/",
            "https://graph.microsoft.com",
        ] {
            refused(post(url));
        }
        // The method still has to be declared.
        refused(screen(
            &e,
            "GET",
            "https://graph.microsoft.com/v1.0/me/events",
            &headers(PH),
            b"",
        ));
        refused(screen(
            &e,
            "DELETE",
            "https://graph.microsoft.com/v1.0/me/events",
            &headers(PH),
            b"",
        ));
    }

    #[test]
    fn a_draft_create_cannot_send_the_draft() {
        let e = exact_call("POST", "/v1.0/me/messages");
        let post = |url: &str| screen(&e, "POST", url, &headers(PH), b"");
        assert!(post("https://graph.microsoft.com/v1.0/me/messages")
            .unwrap()
            .is_some());
        for url in [
            "https://graph.microsoft.com/v1.0/me/messages/AAMk/send",
            "https://graph.microsoft.com/v1.0/me/messages/AAMk/forward",
            "https://graph.microsoft.com/v1.0/me/messages/AAMk/createForward",
            "https://graph.microsoft.com/v1.0/me/sendMail",
        ] {
            let r = refused(post(url));
            assert!(r.text.contains("exactly /v1.0/me/messages"), "{}", r.text);
        }
    }

    #[test]
    fn prefixes_and_exact_paths_in_one_shape_and_in_several() {
        let mut both = shape(&["POST"], "/v1.0/me/messages");
        both.paths = vec!["/v1.0/me/sendMail".into()];
        let e = envelope(Some(ActiveCall::new(
            "c8",
            "t",
            Some(URN.into()),
            vec![both, exact_shape(&["PATCH"], "/v1.0/me/events")],
            Duration::from_secs(60),
        )));
        let ok = |m: &str, p: &str| {
            screen(
                &e,
                m,
                &format!("https://graph.microsoft.com{p}"),
                &headers(PH),
                b"",
            )
            .is_ok()
        };
        assert!(ok("POST", "/v1.0/me/messages/x/send"));
        assert!(ok("POST", "/v1.0/me/sendMail"));
        assert!(!ok("POST", "/v1.0/me/sendMail/x"));
        assert!(ok("PATCH", "/v1.0/me/events"));
        assert!(!ok("PATCH", "/v1.0/me/events/x"));
        assert!(!ok("PATCH", "/v1.0/me/sendMail"));
        assert!(!ok("POST", "/v1.0/me/events"));
    }

    #[test]
    fn a_shape_stored_before_exact_paths_still_reads() {
        let old = r#"{"binding":"graph","host":"graph.microsoft.com","methods":["GET"],"path_prefixes":["/v1.0/me/messages"]}"#;
        let s: Shape = serde_json::from_str(old).unwrap();
        assert!(s.paths.is_empty());
        let back = serde_json::to_string(&s).unwrap();
        assert!(!back.contains("paths\""), "{back}");
    }

    #[test]
    fn exact_rules() {
        let segs = |p: &str| normalize_path(p).unwrap();
        assert!(exact_admits("/a/b", "/a/b", &segs("/a/b")));
        assert!(!exact_admits("/a/b", "/a/b/", &segs("/a/b/")));
        assert!(!exact_admits("/a/b", "/a", &segs("/a")));
        assert!(!exact_admits("/a/b", "/a/b/c", &segs("/a/b/c")));
        // A declared path that publish would refuse admits nothing, even a request equal to it.
        assert!(!exact_admits("/", "/", &segs("/")));
        assert!(!exact_admits("", "/", &segs("/")));
        assert!(!exact_admits("/a/b/", "/a/b", &segs("/a/b")));
        assert!(!exact_admits("/a//b", "/a/b", &segs("/a/b")));
        assert!(!exact_admits("/a/../b", "/b", &segs("/b")));
        assert!(!exact_admits("a/b", "/a/b", &segs("/a/b")));
    }

    #[test]
    fn no_window_undeclared_tool_and_other_skill_are_refused() {
        let url = "https://graph.microsoft.com/v1.0/me/messages";
        refused(get(&envelope(None), url));

        let bare = ActiveCall::new(
            "c3",
            "m365_probe_undeclared",
            Some(URN.into()),
            vec![],
            Duration::from_secs(60),
        );
        let r = refused(get(&envelope(Some(bare)), url));
        assert!(r.text.contains("m365_probe_undeclared"));
        assert_eq!(r.rule, "tool:m365_probe_undeclared egress");

        let no_connector = ActiveCall::new(
            "c4",
            "t",
            None,
            vec![shape(&["GET"], "/")],
            Duration::from_secs(60),
        );
        refused(get(&envelope(Some(no_connector)), url));

        let other_urn = ActiveCall::new(
            "c5",
            "t",
            Some("urn:vam:connector:evil.example/x/microsoft-365".into()),
            vec![shape(&["GET"], "/")],
            Duration::from_secs(60),
        );
        refused(get(&envelope(Some(other_urn)), url));

        let mut other_binding = read_call();
        other_binding.shapes[0].binding = "other".into();
        refused(get(&envelope(Some(other_binding)), url));

        let mut other_host = read_call();
        other_host.shapes[0].host = "evil.example".into();
        refused(get(&envelope(Some(other_host)), url));
    }

    #[test]
    fn an_expired_or_cleared_window_admits_nothing() {
        let url = "https://graph.microsoft.com/v1.0/me/messages";
        let expired = ActiveCall::new(
            "c",
            "t",
            Some(URN.into()),
            vec![shape(&["GET"], "/")],
            Duration::ZERO,
        );
        refused(get(&envelope(Some(expired)), url));

        let e = envelope(Some(read_call()));
        assert!(get(&e, url).unwrap().is_some());
        e.window().clear();
        refused(get(&e, url));
    }

    #[test]
    fn the_placeholder_is_only_valid_as_the_authorization_value() {
        let e = envelope(Some(read_call()));
        let url = "https://graph.microsoft.com/v1.0/me/messages";
        // In the URL, the body, another header, a header name.
        refused(get(&e, &format!("{url}?token={PH}")));
        refused(screen(&e, "GET", url, &headers(PH), PH.as_bytes()));
        let mut h = headers(PH);
        h.insert("X-Echo".into(), PH.into());
        refused(screen(&e, "GET", url, &h, b""));
        let mut h = headers(PH);
        h.insert(PH.into(), "1".into());
        refused(screen(&e, "GET", url, &h, b""));
        // Not exactly the placeholder.
        for v in [
            format!("Basic {PH}"),
            format!("Bearer {PH}x"),
            format!("Bearer {PH} extra"),
            format!("{PH}{PH}"),
            format!("x {PH}"),
        ] {
            refused(screen(&e, "GET", url, &headers(&v), b""));
        }
        // A second Authorization spelling that is not the placeholder.
        let mut h = headers(PH);
        h.insert("authorization".into(), "Bearer evil".into());
        refused(screen(&e, "GET", url, &h, b""));
        // The placeholder somewhere but no Authorization header at all.
        let h = HashMap::from([("X-Echo".to_string(), PH.to_string())]);
        refused(screen(&e, "GET", url, &h, b""));
    }

    #[test]
    fn refusals_name_the_tool_and_never_echo_the_request() {
        let e = envelope(Some(read_call()));
        let r = refused(screen(
            &e,
            "POST",
            "https://graph.microsoft.com/v1.0/me/sendMail",
            &headers(PH),
            b"",
        ));
        assert!(r.text.contains("m365_probe_search"));
        assert!(!r.text.contains("sendMail"));
        assert_eq!(r.rule, "tool:m365_probe_search egress");
    }

    #[test]
    fn recheck_requires_the_same_active_call() {
        let e = envelope(Some(read_call()));
        let grant = get(&e, "https://graph.microsoft.com/v1.0/me/messages")
            .unwrap()
            .unwrap();
        assert!(e.recheck(&grant).is_ok());
        // The next intent replaced the call between authorize and attach.
        e.window().open(ActiveCall::new(
            "c-next",
            "other",
            None,
            vec![],
            Duration::from_secs(60),
        ));
        assert!(e.recheck(&grant).is_err());
        e.window().clear();
        assert!(e.recheck(&grant).is_err());
    }

    #[test]
    fn set_bearer_leaves_one_authorization_header() {
        let mut h = HashMap::from([
            ("authorization".to_string(), PH.to_string()),
            ("AUTHORIZATION".to_string(), PH.to_string()),
            ("X-Keep".to_string(), "1".to_string()),
        ]);
        set_bearer(&mut h, "tok");
        assert_eq!(h.len(), 2);
        assert_eq!(
            h.get("Authorization").map(String::as_str),
            Some("Bearer tok")
        );
    }

    #[test]
    fn prefix_rules() {
        let segs = |p: &str| normalize_path(p).unwrap();
        assert!(prefix_admits("/", &segs("/anything/at/all")));
        assert!(prefix_admits("/a/b/", &segs("/a/b")));
        assert!(prefix_admits("/a/b", &segs("/a/b/")));
        assert!(!prefix_admits("/a/b", &segs("/a")));
        assert!(!prefix_admits("/a/b", &segs("/a/bc")));
    }
}
