//! Remote MCP client: JSON-RPC 2.0 over HTTPS (Streamable HTTP transport).
//!
//! First-party client for vendor-operated MCP endpoints (Jira, Linear, Sentry).
//! Same shape as `host_tools_http::invoke_with_thread_boundary`: resolve a
//! per-caller vault secret, attach it as `Authorization`, pin the URL through
//! [`PinnedUrl`], make the call. No process is spawned.
//!
//! The client is blocking (`reqwest::blocking`); async callers must run it on
//! a blocking thread.

use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::secrets::SecretsVault;
use crate::ssrf_guard::PinnedUrl;

pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
pub const DEFAULT_MCP_TIMEOUT_MS: u64 = 30_000;
const SESSION_HEADER: &str = "Mcp-Session-Id";
const PROTOCOL_HEADER: &str = "MCP-Protocol-Version";

#[derive(Debug, thiserror::Error)]
pub enum McpClientError {
    /// The URL failed SSRF validation (scheme, allowlist, restricted IP).
    #[error("mcp url rejected: {0}")]
    UrlRejected(String),
    /// Connect, TLS, timeout, or unreadable body.
    #[error("mcp transport failure: {0}")]
    Transport(String),
    /// The server returned a non-2xx HTTP status (without exposing its body).
    #[error("mcp transport failure: HTTP {0} from MCP server")]
    HttpStatus(u16),
    /// HTTP 429. `retry_after_ms` is the server's `Retry-After` in
    /// delta-seconds (an HTTP-date is ignored). The body is never exposed.
    #[error("mcp transport failure: HTTP 429 from MCP server (rate limited)")]
    RateLimited { retry_after_ms: Option<u64> },
    /// The server answered with a well-formed JSON-RPC error object.
    #[error("mcp server returned JSON-RPC error {code}: {message}")]
    JsonRpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// The server answered, but not with a valid JSON-RPC 2.0 / MCP response.
    #[error("mcp protocol violation: {0}")]
    Protocol(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "empty_object")]
    pub input_schema: Value,
    /// Human-facing name from the server. Display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Vendor hints. Untrusted: shown and suggested, never enforced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotations>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

impl Default for McpTool {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: None,
            input_schema: empty_object(),
            title: None,
            annotations: None,
            output_schema: None,
        }
    }
}

/// `annotations` of an MCP tool. Every field is a vendor claim.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

fn empty_object() -> Value {
    json!({})
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInitializeResult {
    pub protocol_version: String,
    #[serde(default)]
    pub server_info: Option<Value>,
    #[serde(default)]
    pub capabilities: Option<Value>,
    /// Server-written usage notes. Untrusted vendor text.
    #[serde(default)]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCallToolResult {
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub structured_content: Option<Value>,
}

pub fn encode_request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub fn encode_notification(method: &str) -> Value {
    json!({ "jsonrpc": "2.0", "method": method })
}

/// Decode one JSON-RPC response for request `id` into its `result`.
pub fn decode_response(body: &Value, id: u64) -> Result<Value, McpClientError> {
    let obj = body
        .as_object()
        .ok_or_else(|| McpClientError::Protocol("response is not a JSON object".into()))?;
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpClientError::Protocol(
            "response is missing jsonrpc: \"2.0\"".into(),
        ));
    }
    if obj.get("id").and_then(Value::as_u64) != Some(id) {
        return Err(McpClientError::Protocol(format!(
            "response id {:?} does not match request id {id}",
            obj.get("id")
        )));
    }
    match (obj.get("result"), obj.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(err)) => {
            let code = err.get("code").and_then(Value::as_i64).ok_or_else(|| {
                McpClientError::Protocol("error object has no integer code".into())
            })?;
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Err(McpClientError::JsonRpc {
                code,
                message,
                data: err.get("data").cloned(),
            })
        }
        _ => Err(McpClientError::Protocol(
            "response must carry exactly one of result or error".into(),
        )),
    }
}

/// Pick the response for `id` out of a `text/event-stream` body.
fn decode_sse(text: &str, id: u64) -> Result<Value, McpClientError> {
    let mut data = String::new();
    let mut events = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.is_empty() && !data.is_empty() {
            events.push(std::mem::take(&mut data));
        }
    }
    if !data.is_empty() {
        events.push(data);
    }
    for event in events {
        let Ok(value) = serde_json::from_str::<Value>(&event) else {
            continue;
        };
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            return Ok(value);
        }
    }
    Err(McpClientError::Protocol(format!(
        "event stream carried no response for request id {id}"
    )))
}

fn decode_tools(result: &Value) -> Result<(Vec<McpTool>, Option<String>), McpClientError> {
    let tools = result
        .get("tools")
        .cloned()
        .ok_or_else(|| McpClientError::Protocol("tools/list result has no tools array".into()))?;
    let tools: Vec<McpTool> = serde_json::from_value(tools)
        .map_err(|e| McpClientError::Protocol(format!("malformed tools/list entry: {e}")))?;
    let cursor = result
        .get("nextCursor")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_string);
    Ok((tools, cursor))
}

/// Exact host of a connection URL. Never empty, and never `*` or `*.…`.
///
/// This is the SSRF allowlist for that connection: the client may call this
/// host and no other. An empty or wildcard list is not a valid substitute.
pub fn connection_host(url: &str) -> Result<String, McpClientError> {
    if !url.starts_with("https://") {
        return Err(McpClientError::UrlRejected(
            "remote MCP url must use https".into(),
        ));
    }
    let parsed = url::Url::parse(url)
        .map_err(|e| McpClientError::UrlRejected(format!("Invalid URL: {e}")))?;
    let host = parsed
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| McpClientError::UrlRejected("URL has no host name".to_string()))?;
    if host.contains('*') {
        return Err(McpClientError::UrlRejected(
            "MCP connection host must be an exact host, not a wildcard".into(),
        ));
    }
    Ok(host.to_string())
}

/// One-entry allowlist: the connection URL's own host.
pub fn allowlist_from_connection_url(url: &str) -> Result<Vec<String>, McpClientError> {
    Ok(vec![connection_host(url)?])
}

/// A stored connection may keep talking to its registered host.
/// A different host is rejected before any request is sent.
pub fn enforce_registered_host(
    registered_url: &str,
    request_url: &str,
) -> Result<(), McpClientError> {
    let registered = connection_host(registered_url)?;
    let requested = connection_host(request_url)?;
    if registered.eq_ignore_ascii_case(&requested) {
        Ok(())
    } else {
        Err(McpClientError::UrlRejected(format!(
            "connection is registered for host '{registered}' and cannot call host '{requested}'"
        )))
    }
}

fn reject_lax_allowlist(allowlist: &[String]) -> Result<(), McpClientError> {
    match allowlist {
        [pattern] if !pattern.is_empty() && !pattern.contains('*') => Ok(()),
        _ => Err(McpClientError::UrlRejected(
            "MCP allowlist must be the single exact host of the registered connection".into(),
        )),
    }
}

/// Resolve the `Authorization` header value for `caller_user_id`, the same
/// way `host_tools_http` does: empty caller reads org scope only.
pub fn resolve_auth_header(
    vault: &SecretsVault,
    caller_user_id: &str,
    binding_id: &str,
) -> Result<SecretString, crate::error::OrchestratorError> {
    if caller_user_id.is_empty() {
        vault.decrypt_secret(binding_id)
    } else {
        vault.decrypt_secret_for_caller(caller_user_id, binding_id)
    }
}

pub struct McpClient {
    http: reqwest::blocking::Client,
    url: String,
    /// Connection headers applied on every POST (Authorization and extras).
    /// Protocol-owned headers (`Accept`, session, protocol version) are set
    /// after these so connection headers cannot override them.
    headers: Vec<(String, SecretString)>,
    session_id: Option<String>,
    next_id: u64,
    initialized: bool,
    instructions: Option<String>,
}

impl McpClient {
    /// SSRF-validate and pin `url`, then build a client bound to that IP.
    pub fn connect(
        url: &str,
        allowlist: &[String],
        headers: Vec<(String, SecretString)>,
        timeout_ms: u64,
    ) -> Result<Self, McpClientError> {
        // Test-node seam: Linear's own MCP URL reaches the runner's loopback mock, nothing else.
        #[cfg(feature = "process-guest")]
        if let Some(mapped) = crate::process_mode::map_connector_url(url) {
            return Self::from_builder(base_builder(timeout_ms), mapped.to_string(), headers);
        }
        if !url.starts_with("https://") {
            return Err(McpClientError::UrlRejected(
                "remote MCP url must use https".into(),
            ));
        }
        // Empty and `*` skip or widen the pin in `PinnedUrl`. Refuse both.
        reject_lax_allowlist(allowlist)?;
        let pinned =
            PinnedUrl::validate_and_pin(url, allowlist).map_err(McpClientError::UrlRejected)?;
        let builder = pinned.apply_to_client_builder(base_builder(timeout_ms));
        Self::from_builder(builder, pinned.final_url.to_string(), headers)
    }

    /// Loopback mock servers are unreachable through [`PinnedUrl`].
    #[cfg(test)]
    pub(crate) fn unpinned_for_tests(url: &str, auth_header: Option<SecretString>) -> Self {
        let headers = match auth_header {
            Some(value) => vec![("Authorization".into(), value)],
            None => Vec::new(),
        };
        Self::unpinned_with_headers_for_tests(url, headers)
    }

    /// Loopback mock with an arbitrary header map.
    #[cfg(test)]
    pub(crate) fn unpinned_with_headers_for_tests(
        url: &str,
        headers: Vec<(String, SecretString)>,
    ) -> Self {
        Self::from_builder(base_builder(5_000), url.to_string(), headers)
            .expect("test client builds")
    }

    fn from_builder(
        builder: reqwest::blocking::ClientBuilder,
        url: String,
        headers: Vec<(String, SecretString)>,
    ) -> Result<Self, McpClientError> {
        let http = builder
            .build()
            .map_err(|e| McpClientError::Transport(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            http,
            url,
            headers,
            session_id: None,
            next_id: 1,
            initialized: false,
            instructions: None,
        })
    }

    pub fn initialize(&mut self) -> Result<McpInitializeResult, McpClientError> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "vamd", "version": env!("CARGO_PKG_VERSION") },
            }),
        )?;
        let init: McpInitializeResult = serde_json::from_value(result)
            .map_err(|e| McpClientError::Protocol(format!("malformed initialize result: {e}")))?;
        self.post(&encode_notification("notifications/initialized"))?;
        self.initialized = true;
        self.instructions = init.instructions.clone();
        Ok(init)
    }

    /// `instructions` from `initialize`, once the session is initialized.
    pub fn server_instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    /// All pages of `tools/list`. Initializes the session on first use.
    pub fn list_tools(&mut self) -> Result<Vec<McpTool>, McpClientError> {
        self.ensure_initialized()?;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let (page, next) = decode_tools(&self.request("tools/list", params)?)?;
            tools.extend(page);
            match next {
                Some(c) if Some(&c) != cursor.as_ref() => cursor = Some(c),
                _ => return Ok(tools),
            }
        }
    }

    pub fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<McpCallToolResult, McpClientError> {
        self.call_tool_with_meta(name, arguments, None)
    }

    /// `tools/call` carrying the effect journal's idempotency key in
    /// `params._meta["io.vam/idempotencyKey"]` (`_meta` is allowed on any request).
    pub fn call_tool_with_meta(
        &mut self,
        name: &str,
        arguments: Value,
        idempotency_key: Option<&str>,
    ) -> Result<McpCallToolResult, McpClientError> {
        self.ensure_initialized()?;
        let mut params = json!({ "name": name, "arguments": arguments });
        if let Some(key) = idempotency_key {
            params["_meta"] = json!({ "io.vam/idempotencyKey": key });
        }
        let result = self.request("tools/call", params)?;
        serde_json::from_value(result)
            .map_err(|e| McpClientError::Protocol(format!("malformed tools/call result: {e}")))
    }

    fn ensure_initialized(&mut self) -> Result<(), McpClientError> {
        if !self.initialized {
            self.initialize()?;
        }
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, McpClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let resp = self.post(&encode_request(id, method, params))?;
        if let Some(sid) = resp
            .headers()
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(sid.to_string());
        }
        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/event-stream"));
        let text = resp
            .text()
            .map_err(|e| McpClientError::Transport(format!("failed to read body: {e}")))?;
        let body = if is_sse {
            decode_sse(&text, id)?
        } else {
            serde_json::from_str(&text)
                .map_err(|e| McpClientError::Protocol(format!("response is not JSON: {e}")))?
        };
        decode_response(&body, id)
    }

    fn post(&self, body: &Value) -> Result<reqwest::blocking::Response, McpClientError> {
        // User headers first, then protocol-owned headers so Accept / session /
        // protocol version always win (banned names are also rejected at connect).
        let mut req = self.http.post(&self.url).json(body);
        for (name, value) in &self.headers {
            req = req.header(name.as_str(), value.expose_secret().as_str());
        }
        req = req.header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        );
        if let Some(sid) = &self.session_id {
            req = req.header(SESSION_HEADER, sid);
        }
        if self.initialized {
            req = req.header(PROTOCOL_HEADER, MCP_PROTOCOL_VERSION);
        }
        let resp = req
            .send()
            .map_err(|e| McpClientError::Transport(format!("request failed: {e}")))?;
        let status = resp.status();
        if status.as_u16() == 429 {
            let retry_after_ms = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after_ms);
            return Err(McpClientError::RateLimited { retry_after_ms });
        }
        if !status.is_success() {
            return Err(McpClientError::HttpStatus(status.as_u16()));
        }
        Ok(resp)
    }
}

/// `Retry-After` as delta-seconds. An HTTP-date or anything else is `None`.
fn parse_retry_after_ms(value: &str) -> Option<u64> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(|seconds| seconds.saturating_mul(1000))
}

fn base_builder(timeout_ms: u64) -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .connect_timeout(Duration::from_millis(5000))
        // A 302 to another host would skip the allowlist. Do not follow it.
        .redirect(reqwest::redirect::Policy::none())
}

#[cfg(test)]
pub(crate) mod mock {
    //! Minimal HTTP/1.1 MCP server on loopback for tests.

    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use serde_json::{json, Value};

    #[derive(Default)]
    pub struct Seen {
        pub methods: Vec<String>,
        pub auth: Vec<Option<String>>,
        pub session: Vec<Option<String>>,
        /// Lowercase header name → value, excluding hop-by-hop noise.
        pub headers: Vec<std::collections::BTreeMap<String, String>>,
    }

    /// `respond(method, id, params)` returns the JSON-RPC body to send, or
    /// `None` for 202 Accepted. Serves until the test process exits.
    pub fn spawn<F>(respond: F) -> (String, Arc<Mutex<Seen>>)
    where
        F: Fn(&str, &Value, &Value) -> Option<(String, String)> + Send + 'static,
    {
        spawn_with_status(move |method, id, params| {
            respond(method, id, params).map(|(ct, body)| (200, ct, body))
        })
    }

    /// Like `spawn`, but lets a mock reply with a non-2xx HTTP status.
    pub fn spawn_with_status<F>(respond: F) -> (String, Arc<Mutex<Seen>>)
    where
        F: Fn(&str, &Value, &Value) -> Option<(u16, String, String)> + Send + 'static,
    {
        spawn_full(move |method, id, params| {
            respond(method, id, params).map(|(status, ct, body)| (status, ct, body, Vec::new()))
        })
    }

    /// Like `spawn_with_status`, plus extra response headers (`Retry-After`).
    pub fn spawn_full<F>(respond: F) -> (String, Arc<Mutex<Seen>>)
    where
        F: Fn(&str, &Value, &Value) -> Option<(u16, String, String, Vec<(String, String)>)>
            + Send
            + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Seen::default()));
        let seen_thread = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let (mut len, mut auth, mut session) = (0usize, None, None);
                let mut request_headers = std::collections::BTreeMap::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    let l = line.trim_end();
                    if l.is_empty() {
                        break;
                    }
                    let (k, v) = l.split_once(':').unwrap();
                    let v = v.trim().to_string();
                    let key = k.to_ascii_lowercase();
                    match key.as_str() {
                        "content-length" => len = v.parse().unwrap(),
                        "authorization" => {
                            auth = Some(v.clone());
                            request_headers.insert(key, v);
                        }
                        "mcp-session-id" => {
                            session = Some(v.clone());
                            request_headers.insert(key, v);
                        }
                        "host"
                        | "connection"
                        | "accept"
                        | "content-type"
                        | "user-agent"
                        | "mcp-protocol-version" => {
                            request_headers.insert(key, v);
                        }
                        _ => {
                            request_headers.insert(key, v);
                        }
                    }
                }
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).unwrap();
                let req: Value = serde_json::from_slice(&body).unwrap();
                let method = req["method"].as_str().unwrap_or_default().to_string();
                {
                    let mut s = seen_thread.lock().unwrap();
                    s.methods.push(method.clone());
                    s.auth.push(auth);
                    s.session.push(session);
                    s.headers.push(request_headers);
                }
                let reply = respond(&method, &req["id"], &req["params"]);
                let out = match reply {
                    None => {
                        "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    }
                    Some((status, ct, body, extra)) => {
                        let reason = reqwest::StatusCode::from_u16(status)
                            .ok()
                            .and_then(|status| status.canonical_reason())
                            .unwrap_or("Unknown");
                        let extra: String = extra
                            .iter()
                            .map(|(name, value)| format!("{name}: {value}\r\n"))
                            .collect();
                        format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Type: {ct}\r\nMcp-Session-Id: sess-1\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                };
                let _ = stream.write_all(out.as_bytes());
            }
        });
        (url, seen)
    }

    pub fn ok(id: &Value, result: Value) -> Option<(String, String)> {
        Some((
            "application/json".into(),
            json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
        ))
    }

    /// Standard server: two tools, echo on call.
    pub fn standard(method: &str, id: &Value, params: &Value) -> Option<(String, String)> {
        match method {
            "initialize" => ok(
                id,
                json!({ "protocolVersion": super::MCP_PROTOCOL_VERSION, "capabilities": {}, "serverInfo": { "name": "mock" } }),
            ),
            "notifications/initialized" => None,
            "tools/list" => ok(
                id,
                json!({ "tools": [
                    { "name": "create_issue", "description": "Create", "inputSchema": { "type": "object" } },
                    { "name": "list_issues", "inputSchema": { "type": "object" } }
                ]}),
            ),
            "tools/call" => ok(
                id,
                json!({ "content": [{ "type": "text", "text": params["name"] }] }),
            ),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_jsonrpc_request_and_notification() {
        let req = encode_request(7, "tools/call", json!({ "name": "x" }));
        assert_eq!(
            req,
            json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": "x" } })
        );
        let n = encode_notification("notifications/initialized");
        assert!(n.get("id").is_none());
    }

    #[test]
    fn decodes_result() {
        let r = decode_response(
            &json!({ "jsonrpc": "2.0", "id": 3, "result": { "ok": true } }),
            3,
        )
        .unwrap();
        assert_eq!(r, json!({ "ok": true }));
    }

    #[test]
    fn decodes_jsonrpc_error_distinctly() {
        let err = decode_response(
            &json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32601, "message": "nope", "data": 5 } }),
            1,
        )
        .unwrap_err();
        match err {
            McpClientError::JsonRpc {
                code,
                message,
                data,
            } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "nope");
                assert_eq!(data, Some(json!(5)));
            }
            other => panic!("expected JsonRpc, got {other}"),
        }
    }

    #[test]
    fn protocol_violations() {
        let cases = [
            json!([1, 2]),
            json!({ "id": 1, "result": {} }),
            json!({ "jsonrpc": "2.0", "id": 2, "result": {} }),
            json!({ "jsonrpc": "2.0", "id": 1 }),
            json!({ "jsonrpc": "2.0", "id": 1, "result": {}, "error": { "code": 1 } }),
            json!({ "jsonrpc": "2.0", "id": 1, "error": { "message": "no code" } }),
        ];
        for case in cases {
            assert!(
                matches!(decode_response(&case, 1), Err(McpClientError::Protocol(_))),
                "{case}"
            );
        }
        assert!(matches!(
            decode_tools(&json!({})),
            Err(McpClientError::Protocol(_))
        ));
        assert!(matches!(
            decode_tools(&json!({ "tools": [{ "description": "no name" }] })),
            Err(McpClientError::Protocol(_))
        ));
    }

    #[test]
    fn decodes_sse_body() {
        let text = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{}}\n\n";
        let v = decode_sse(text, 4).unwrap();
        assert_eq!(decode_response(&v, 4).unwrap(), json!({}));
        assert!(matches!(
            decode_sse(text, 9),
            Err(McpClientError::Protocol(_))
        ));
    }

    #[test]
    fn connect_rejects_loopback_and_link_local() {
        for url in [
            "https://127.0.0.1:9/mcp",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/mcp",
            "https://localhost/mcp",
        ] {
            let host = connection_host(url).unwrap();
            let Err(err) = McpClient::connect(url, &[host], vec![], 1_000) else {
                panic!("{url} was accepted");
            };
            let McpClientError::UrlRejected(msg) = err else {
                panic!("{url}");
            };
            assert!(
                msg.contains("Restricted") || msg.contains("restricted") || msg.contains("SSRF"),
                "{url}: {msg}"
            );
        }
        for url in ["http://mcp.linear.app/mcp", "file:///etc/passwd"] {
            assert!(
                matches!(
                    McpClient::connect(url, &["mcp.linear.app".into()], vec![], 1_000),
                    Err(McpClientError::UrlRejected(_))
                ),
                "{url}"
            );
        }
    }

    #[test]
    fn connection_registered_for_host_a_cannot_call_host_b() {
        let registered = "https://mcp-a.example/mcp";
        let allowlist = allowlist_from_connection_url(registered).unwrap();
        assert_eq!(allowlist, vec!["mcp-a.example".to_string()]);
        assert_eq!(
            connection_host("https://MCP-A.example/other").unwrap(),
            "mcp-a.example"
        );
        enforce_registered_host(registered, "https://mcp-a.example/v2").unwrap();

        let moved = enforce_registered_host(registered, "https://mcp-b.example/mcp").unwrap_err();
        let McpClientError::UrlRejected(msg) = moved else {
            panic!("expected UrlRejected");
        };
        assert!(
            msg.contains("mcp-a.example") && msg.contains("mcp-b.example"),
            "{msg}"
        );

        let Err(err) = McpClient::connect("https://mcp-b.example/mcp", &allowlist, vec![], 1_000)
        else {
            panic!("host B was accepted");
        };
        let McpClientError::UrlRejected(msg) = err else {
            panic!("expected UrlRejected");
        };
        assert!(msg.contains("mcp-b.example"), "{msg}");

        for lax in [
            Vec::new(),
            vec!["*".to_string()],
            vec!["*.example".to_string()],
        ] {
            let Err(err) = McpClient::connect("https://mcp-b.example/mcp", &lax, vec![], 1_000)
            else {
                panic!("{lax:?} was accepted");
            };
            assert!(
                matches!(err, McpClientError::UrlRejected(_)),
                "{lax:?} -> {err}"
            );
        }
    }

    #[test]
    fn redirect_to_another_host_is_not_followed() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let sink = TcpListener::bind("127.0.0.1:0").unwrap();
        sink.set_nonblocking(true).unwrap();
        let sink_addr = sink.local_addr().unwrap();

        let redirector = TcpListener::bind("127.0.0.1:0").unwrap();
        let redirect_url = format!("http://{}/mcp", redirector.local_addr().unwrap());
        let location = format!("http://{sink_addr}/stolen");
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = redirector.accept() else {
                return;
            };
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let body = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(body.as_bytes());
        });

        let mut client = McpClient::unpinned_for_tests(
            &redirect_url,
            Some(SecretString::new("Bearer secret-token".into())),
        );
        let err = client.initialize().unwrap_err();
        assert!(matches!(err, McpClientError::HttpStatus(302)), "{err}");
        assert!(!err.to_string().contains("secret-token"), "{err}");
        assert!(
            sink.accept().is_err(),
            "redirect target {sink_addr} was contacted"
        );
    }

    #[test]
    fn full_session_against_mock_server() {
        let (url, seen) = mock::spawn(mock::standard);
        let mut client =
            McpClient::unpinned_for_tests(&url, Some(SecretString::new("Bearer tok".into())));
        let tools = client.list_tools().unwrap();
        assert_eq!(
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["create_issue", "list_issues"]
        );
        let out = client.call_tool("create_issue", json!({})).unwrap();
        assert_eq!(out.content[0]["text"], "create_issue");
        assert!(!out.is_error);

        let s = seen.lock().unwrap();
        assert_eq!(
            s.methods,
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call"
            ]
        );
        assert!(s.auth.iter().all(|a| a.as_deref() == Some("Bearer tok")));
        assert_eq!(s.session[0], None);
        assert_eq!(s.session[3].as_deref(), Some("sess-1"));
    }

    #[test]
    fn server_error_and_http_failure_are_distinct() {
        let (url, _) = mock::spawn(|method, id, _| {
            match method {
            "tools/list" => Some((
                "application/json".into(),
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": "denied" } })
                    .to_string(),
            )),
            other => mock::standard(other, id, &Value::Null),
        }
        });
        let mut client = McpClient::unpinned_for_tests(&url, None);
        assert!(matches!(
            client.list_tools(),
            Err(McpClientError::JsonRpc { code: -32000, .. })
        ));

        let mut dead = McpClient::unpinned_for_tests("http://127.0.0.1:1/mcp", None);
        assert!(matches!(
            dead.list_tools(),
            Err(McpClientError::Transport(_))
        ));
    }

    #[test]
    fn tools_call_unauthorized_is_typed_and_does_not_expose_secrets() {
        let (url, seen) = mock::spawn_with_status(|method, id, params| {
            if method == "tools/call" {
                Some((401, "text/plain".into(), "private-response-body".into()))
            } else {
                mock::standard(method, id, params).map(|(ct, body)| (200, ct, body))
            }
        });
        let mut client = McpClient::unpinned_for_tests(
            &url,
            Some(SecretString::new("Bearer private-token".into())),
        );
        let err = client.call_tool("create_issue", json!({})).unwrap_err();
        assert!(matches!(err, McpClientError::HttpStatus(401)), "{err}");
        assert_eq!(
            err.to_string(),
            "mcp transport failure: HTTP 401 from MCP server"
        );
        assert!(!err.to_string().contains("private-response-body"));
        assert!(!err.to_string().contains("private-token"));
        assert_eq!(
            seen.lock().unwrap().methods,
            ["initialize", "notifications/initialized", "tools/call"]
        );
    }

    #[test]
    fn rate_limit_exposes_retry_after_and_not_the_body() {
        for (header, expected) in [
            (Some("3"), Some(3000)),
            (Some("0"), Some(0)),
            (Some("Wed, 21 Oct 2026 07:28:00 GMT"), None),
            (None, None),
        ] {
            let header = header.map(str::to_string);
            let (url, _seen) = mock::spawn_full(move |method, id, params| {
                if method == "tools/call" {
                    let extra = header
                        .iter()
                        .map(|v| ("Retry-After".to_string(), v.clone()))
                        .collect();
                    Some((429, "text/plain".into(), "slow-down-body".into(), extra))
                } else {
                    mock::standard(method, id, params).map(|(ct, body)| (200, ct, body, vec![]))
                }
            });
            let mut client = McpClient::unpinned_for_tests(&url, None);
            let err = client.call_tool("create_issue", json!({})).unwrap_err();
            assert!(
                matches!(err, McpClientError::RateLimited { retry_after_ms } if retry_after_ms == expected),
                "{err:?}"
            );
            assert!(!err.to_string().contains("slow-down-body"));
        }
    }

    #[test]
    fn server_error_stays_a_plain_http_status() {
        let (url, _seen) = mock::spawn_with_status(|method, id, params| {
            if method == "tools/call" {
                Some((503, "text/plain".into(), "down".into()))
            } else {
                mock::standard(method, id, params).map(|(ct, body)| (200, ct, body))
            }
        });
        let mut client = McpClient::unpinned_for_tests(&url, None);
        let err = client.call_tool("create_issue", json!({})).unwrap_err();
        assert!(matches!(err, McpClientError::HttpStatus(503)), "{err:?}");
    }

    #[test]
    fn multi_header_attach_on_every_post() {
        let (url, seen) = mock::spawn(mock::standard);
        let mut client = McpClient::unpinned_with_headers_for_tests(
            &url,
            vec![
                (
                    "Authorization".into(),
                    SecretString::new("Bearer tok".into()),
                ),
                (
                    "Ocp-Apim-Subscription-Key".into(),
                    SecretString::new("sub-key-1".into()),
                ),
                (
                    "x-functions-key".into(),
                    SecretString::new("fn-key-2".into()),
                ),
            ],
        );
        let _ = client.list_tools().unwrap();
        let out = client.call_tool("create_issue", json!({})).unwrap();
        assert_eq!(out.content[0]["text"], "create_issue");

        let s = seen.lock().unwrap();
        assert!(s.methods.contains(&"tools/list".to_string()));
        assert!(s.methods.contains(&"tools/call".to_string()));
        for headers in &s.headers {
            assert_eq!(
                headers.get("authorization").map(String::as_str),
                Some("Bearer tok")
            );
            assert_eq!(
                headers.get("ocp-apim-subscription-key").map(String::as_str),
                Some("sub-key-1")
            );
            assert_eq!(
                headers.get("x-functions-key").map(String::as_str),
                Some("fn-key-2")
            );
            // Protocol Accept is set after user headers and must be present.
            assert!(headers.get("accept").is_some());
        }
    }

    #[test]
    fn paginates_tools_list() {
        let (url, _) = mock::spawn(|method, id, params| match method {
            "tools/list" if params.get("cursor").is_none() => mock::ok(
                id,
                json!({ "tools": [{ "name": "a" }], "nextCursor": "p2" }),
            ),
            "tools/list" => mock::ok(id, json!({ "tools": [{ "name": "b" }] })),
            other => mock::standard(other, id, params),
        });
        let mut client = McpClient::unpinned_for_tests(&url, None);
        let names: Vec<_> = client
            .list_tools()
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["a", "b"]);
    }
}
