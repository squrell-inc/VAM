use crate::cache::{is_cache_related_error, note_downgrade, CacheMode, CachePlan, CacheTtl};
use crate::catalog::ModelCatalogEntry;
use crate::error::VamInferenceError;
use crate::mock_script::{self, MockMatchError, MockPlan, MockScripts};
use crate::wire::WireOptions;
use async_trait::async_trait;
use futures_util::StreamExt;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use vam_spec::v1::{
    ErrorStatus, InferenceRequest, InferenceResponseChunk, TokenUsage, ToolCall, ToolSpec,
    VamErrorCode,
};

/// OpenAI-compatible and Anthropic function names match `^[A-Za-z0-9_-]{1,64}$`.
const PROVIDER_TOOL_NAME_MAX_LEN: usize = 64;
const WIRE_ALIAS_HASH_HEX_LEN: usize = 8;
const WIRE_ALIAS_PREFIX_MAX_LEN: usize = PROVIDER_TOOL_NAME_MAX_LEN - 1 - WIRE_ALIAS_HASH_HEX_LEN;

/// Name sent to the model provider.
///
/// `.` becomes `_` so HSP tools (`echo.ping`) stay valid. A name of 64
/// characters or fewer is otherwise unchanged. A longer name is
/// `{prefix}_{8 hex}`: `prefix` is the leading provider-safe characters (at
/// most 55) and the hex is SHA-256 of the full guest name. Cedar, audit, and
/// the guest keep that full name. A returned tool call is mapped back with
/// `wire_to_canonical_names` before it leaves the broker.
fn provider_wire_tool_name(name: &str) -> String {
    let safe = name.replace('.', "_");
    if safe.chars().count() <= PROVIDER_TOOL_NAME_MAX_LEN {
        return safe;
    }
    let prefix = provider_safe_prefix(&safe, WIRE_ALIAS_PREFIX_MAX_LEN);
    let hash = sha256_hex_prefix(name.as_bytes(), WIRE_ALIAS_HASH_HEX_LEN);
    format!("{prefix}_{hash}")
}

fn provider_safe_prefix(safe: &str, max_chars: usize) -> String {
    let mut prefix = String::new();
    for ch in safe.chars().take(max_chars) {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            prefix.push(ch);
        } else {
            prefix.push('_');
        }
    }
    if prefix.is_empty() {
        "tool".to_string()
    } else {
        prefix
    }
}

fn sha256_hex_prefix(bytes: &[u8], hex_len: usize) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    if hex.len() > hex_len {
        hex.truncate(hex_len);
    }
    hex
}

fn wire_to_canonical_names(tools: &[ToolSpec]) -> HashMap<String, String> {
    tools
        .iter()
        .map(|tool| (provider_wire_tool_name(&tool.name), tool.name.clone()))
        .collect()
}

fn canonical_tool_name(wire_to_canonical: &HashMap<String, String>, wire: &str) -> String {
    wire_to_canonical
        .get(wire)
        .cloned()
        .unwrap_or_else(|| wire.to_string())
}

fn tool_parameters_schema(schema_json: &str) -> serde_json::Value {
    if schema_json.is_empty() {
        return serde_json::json!({"type": "object"});
    }
    serde_json::from_str(schema_json).unwrap_or_else(|_| serde_json::json!({"type": "object"}))
}

/// Bounded retry for provider calls. Inference has no external effect, so a
/// request that failed before the first streamed byte may be sent again.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts, the first included.
    pub max_attempts: u32,
    /// Wait before the second attempt; each later wait is four times longer.
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(250),
        }
    }
}

/// The longest wait a `retry-after` header can ask for.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(5);

/// 429, 529 (Anthropic overloaded) and 5xx.
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 429 || status.as_u16() == 529 || status.is_server_error()
}

/// Sends the request, again on a retryable status while attempts remain. The
/// last response is returned as it is, so the caller reports its body.
async fn send_with_retry<F>(
    policy: RetryPolicy,
    make_request: F,
) -> Result<reqwest::Response, reqwest::Error>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let attempts = policy.max_attempts.max(1);
    let mut attempt = 1u32;
    loop {
        let resp = make_request().send().await?;
        if !is_retryable_status(resp.status()) || attempt >= attempts {
            return Ok(resp);
        }
        let asked = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|secs| Duration::from_secs(secs).min(RETRY_AFTER_CAP));
        let backoff = policy
            .base_delay
            .saturating_mul(4u32.saturating_pow(attempt - 1));
        let wait = asked.unwrap_or(backoff).min(RETRY_AFTER_CAP);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        attempt += 1;
    }
}

#[async_trait]
pub trait InferenceProvider: Send + Sync + std::fmt::Debug {
    async fn infer(
        &self,
        request: InferenceRequest,
        catalog_entry: ModelCatalogEntry,
        api_key: Option<SecretString>,
        plan: &CachePlan,
    ) -> Result<mpsc::Receiver<Result<InferenceResponseChunk, VamInferenceError>>, VamInferenceError>;

    /// Like `infer`, with the run's harness options for this request (thinking mode,
    /// `tool_choice`, effort, reasoning to re-attach). Only the OpenAI-compatible
    /// provider acts on them; the default ignores them.
    async fn infer_with(
        &self,
        request: InferenceRequest,
        catalog_entry: ModelCatalogEntry,
        api_key: Option<SecretString>,
        plan: &CachePlan,
        _wire: &WireOptions,
    ) -> Result<mpsc::Receiver<Result<InferenceResponseChunk, VamInferenceError>>, VamInferenceError>
    {
        self.infer(request, catalog_entry, api_key, plan).await
    }
}

/// The host-side mock model. A request whose messages carry a `mock-script:<id>` marker is answered
/// from that script (see `mock_script`). Every other request gets the content fixtures below.
#[derive(Clone, Default)]
pub struct MockProvider {
    scripts: Option<Arc<MockScripts>>,
    trace: Option<mock_script::MockTraceCallback>,
}

impl std::fmt::Debug for MockProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockProvider")
            .field("scripts", &self.scripts)
            .field("trace", &self.trace.is_some())
            .finish()
    }
}

impl MockProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// A provider that answers scripted requests from `scripts`.
    pub fn with_scripts(scripts: Arc<MockScripts>) -> Self {
        Self {
            scripts: Some(scripts),
            trace: None,
        }
    }

    /// Reports one `MockTrace` per scripted request to `trace`.
    pub fn with_trace(mut self, trace: Option<mock_script::MockTraceCallback>) -> Self {
        self.trace = trace;
        self
    }
}

/// Streams a scripted reply, or the error chunk that says why there is none.
async fn stream_scripted_reply(
    tx: mpsc::Sender<Result<InferenceResponseChunk, VamInferenceError>>,
    scripts: Option<Arc<MockScripts>>,
    trace: Option<mock_script::MockTraceCallback>,
    script_id: String,
    request: InferenceRequest,
    model: String,
) {
    let (planned, event) = match scripts.as_deref().and_then(|loaded| loaded.get(&script_id)) {
        Some(script) => mock_script::plan_with_trace_for(
            script,
            &request.tools,
            &request.messages,
            vam_spec::compaction::is_resample_hint(request.context_hint.as_ref()),
            Some(model.as_str()),
        ),
        None => {
            let err = MockMatchError::NotLoaded { id: script_id };
            let event = mock_script::unmatched_trace(&err, &request.messages);
            (Err(err), event)
        }
    };
    if let Some(trace) = &trace {
        trace(&event);
    }
    match planned {
        Ok(plan) if plan.fail_status.is_some() => {
            let status = plan.fail_status.unwrap_or(500);
            let _ = tx
                .send(Err(VamInferenceError::ProviderError {
                    provider: "mock".to_string(),
                    details: mock_failure_details(status),
                }))
                .await;
        }
        Ok(plan) => stream_plan(&tx, plan).await,
        Err(err) => {
            let chunk = InferenceResponseChunk {
                delta: String::new(),
                tool_calls: vec![],
                usage: None,
                error: Some(ErrorStatus {
                    code: VamErrorCode::Internal as i32,
                    message: err.to_string(),
                    retryable: false,
                    details: vec![],
                }),
                ..Default::default()
            };
            let _ = tx.send(Ok(chunk)).await;
        }
    }
}

/// The failure text a real provider's error carries: `HTTP Status <code> <reason>: <body>`.
fn mock_failure_details(status: u16) -> String {
    let code = reqwest::StatusCode::from_u16(status).unwrap_or(reqwest::StatusCode::BAD_GATEWAY);
    format!("HTTP Status {code}: scripted failure")
}

/// Sends a planned reply as text deltas, then its tool call, then the usage chunk.
async fn stream_plan(
    tx: &mpsc::Sender<Result<InferenceResponseChunk, VamInferenceError>>,
    plan: MockPlan,
) {
    let delayed = plan.delay_ms > 0;
    let pause = Duration::from_millis(plan.delay_ms / mock_script::DELAY_PIECES as u64);
    // The reply's reasoning goes first, as a reasoning model streams it.
    if let Some(reasoning) = plan.reasoning.as_deref().filter(|r| !r.is_empty()) {
        let chunk = InferenceResponseChunk {
            reasoning: reasoning.to_string(),
            ..Default::default()
        };
        if tx.send(Ok(chunk)).await.is_err() {
            return;
        }
    }
    for piece in mock_script::stream_pieces(&plan.text, delayed) {
        let chunk = InferenceResponseChunk {
            delta: piece.to_string(),
            tool_calls: vec![],
            usage: None,
            error: None,
            ..Default::default()
        };
        if tx.send(Ok(chunk)).await.is_err() {
            return;
        }
        if delayed {
            tokio::time::sleep(pause).await;
        }
    }
    if let Some(call) = plan.tool_call {
        let chunk = InferenceResponseChunk {
            delta: String::new(),
            tool_calls: vec![call],
            usage: None,
            error: None,
            ..Default::default()
        };
        if tx.send(Ok(chunk)).await.is_err() {
            return;
        }
    }
    let usage = TokenUsage {
        prompt_tokens: plan.prompt_tokens,
        completion_tokens: plan.completion_tokens,
        cached_tokens: 0,
        cache_creation_tokens: 0,
        cached_tokens_unknown: false,
        cache_creation_long_tokens: 0,
        total_tokens: plan.prompt_tokens.saturating_add(plan.completion_tokens),
    };
    let final_chunk = InferenceResponseChunk {
        delta: String::new(),
        tool_calls: vec![],
        usage: Some(usage),
        error: None,
        ..Default::default()
    };
    let _ = tx.send(Ok(final_chunk)).await;
}

#[async_trait]
impl InferenceProvider for MockProvider {
    async fn infer(
        &self,
        request: InferenceRequest,
        catalog_entry: ModelCatalogEntry,
        _api_key: Option<SecretString>,
        _plan: &CachePlan,
    ) -> Result<mpsc::Receiver<Result<InferenceResponseChunk, VamInferenceError>>, VamInferenceError>
    {
        let (tx, rx) = mpsc::channel(32);

        // Check if there are any messages
        if request.messages.is_empty() {
            return Err(VamInferenceError::InvalidRequest {
                reason: "Empty messages list".to_string(),
            });
        }

        // A marker selects a script, and the script answers even when a fixture word also appears.
        if let Some(script_id) = mock_script::requested_script(&request.messages) {
            let scripts = self.scripts.clone();
            let trace = self.trace.clone();
            tokio::spawn(stream_scripted_reply(
                tx,
                scripts,
                trace,
                script_id,
                request,
                catalog_entry.model_name.clone(),
            ));
            return Ok(rx);
        }

        let playbook_fixture = request
            .messages
            .iter()
            .any(|message| message.content.contains("vam-playbook-finish-fixture"));
        let saw_missing_steps = request
            .messages
            .iter()
            .any(|message| message.content.contains("Missing required steps"));
        let nudge_probe = request
            .messages
            .iter()
            .any(|message| message.content.contains("vam-nudge-probe"));
        let user_messages = request
            .messages
            .iter()
            .filter(|message| message.role == "user")
            .count();
        let last_message = request.messages.last().unwrap();
        let role = last_message.role.clone();
        let content = last_message.content.clone();
        let tool_call_id = last_message.tool_call_id.clone();

        tokio::spawn(async move {
            if nudge_probe {
                // Answers in prose that sounds like deferred tool intent and reports how many user
                // messages the conversation holds, so a test can see whether the runtime added any.
                let chunk = InferenceResponseChunk {
                    delta: format!(
                        "I'll check that and fetch the details. user_messages={user_messages}"
                    ),
                    tool_calls: vec![],
                    usage: Some(TokenUsage {
                        prompt_tokens: 8,
                        completion_tokens: 8,
                        cached_tokens: 0,
                        cache_creation_tokens: 0,
                        cached_tokens_unknown: false,
                        cache_creation_long_tokens: 0,
                        total_tokens: 16,
                    }),
                    error: None,
                    ..Default::default()
                };
                let _ = tx.send(Ok(chunk)).await;
            } else if playbook_fixture {
                // Scripted playbook finish: prose, then step_complete after the
                // host observation, then a JSON run output.
                if role == "tool" {
                    let chunk = InferenceResponseChunk {
                        delta: r#"{"ok":true}"#.to_string(),
                        tool_calls: vec![],
                        usage: Some(TokenUsage {
                            prompt_tokens: 10,
                            completion_tokens: 4,
                            cached_tokens: 0,
                            cache_creation_tokens: 0,
                            cached_tokens_unknown: false,
                            cache_creation_long_tokens: 0,
                            total_tokens: 14,
                        }),
                        error: None,
                        ..Default::default()
                    };
                    let _ = tx.send(Ok(chunk)).await;
                } else if saw_missing_steps {
                    let tc = ToolCall {
                        id: "call_step_complete_1".to_string(),
                        name: "step_complete".to_string(),
                        arguments: r#"{"step_id":"first","outcome":"done"}"#.to_string(),
                    };
                    let chunk = InferenceResponseChunk {
                        delta: String::new(),
                        tool_calls: vec![tc],
                        usage: Some(TokenUsage {
                            prompt_tokens: 12,
                            completion_tokens: 8,
                            cached_tokens: 0,
                            cache_creation_tokens: 0,
                            cached_tokens_unknown: false,
                            cache_creation_long_tokens: 0,
                            total_tokens: 20,
                        }),
                        error: None,
                        ..Default::default()
                    };
                    let _ = tx.send(Ok(chunk)).await;
                } else {
                    let chunk = InferenceResponseChunk {
                        delta: "finishing early".to_string(),
                        tool_calls: vec![],
                        usage: Some(TokenUsage {
                            prompt_tokens: 8,
                            completion_tokens: 3,
                            cached_tokens: 0,
                            cache_creation_tokens: 0,
                            cached_tokens_unknown: false,
                            cache_creation_long_tokens: 0,
                            total_tokens: 11,
                        }),
                        error: None,
                        ..Default::default()
                    };
                    let _ = tx.send(Ok(chunk)).await;
                }
            } else if role == "user" && content.contains("run-echo") {
                // 1. Return a tool call to 'echo'
                let tc = ToolCall {
                    id: "call_echo_123".to_string(),
                    name: "echo".to_string(),
                    arguments: "{\"query\": \"hello from the VM\"}".to_string(),
                };
                let chunk1 = InferenceResponseChunk {
                    delta: String::new(),
                    tool_calls: vec![tc],
                    usage: None,
                    error: None,
                    ..Default::default()
                };
                let _ = tx.send(Ok(chunk1)).await;

                // Send a second chunk containing usage info to end the stream
                let usage = TokenUsage {
                    prompt_tokens: 15,
                    completion_tokens: 25,
                    cached_tokens: 0,
                    cache_creation_tokens: 0,
                    cached_tokens_unknown: false,
                    cache_creation_long_tokens: 0,
                    total_tokens: 40,
                };
                let chunk2 = InferenceResponseChunk {
                    delta: String::new(),
                    tool_calls: vec![],
                    usage: Some(usage),
                    error: None,
                    ..Default::default()
                };
                let _ = tx.send(Ok(chunk2)).await;
            } else if role == "tool" && tool_call_id == "call_echo_123" {
                // 2. Return the final answer following the tool call
                let words = ["The ", "tool ", "echo ", "returned ", "success."];
                for word in words {
                    let chunk = InferenceResponseChunk {
                        delta: word.to_string(),
                        tool_calls: vec![],
                        usage: None,
                        error: None,
                        ..Default::default()
                    };
                    let _ = tx.send(Ok(chunk)).await;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }

                let usage = TokenUsage {
                    prompt_tokens: 50,
                    completion_tokens: 15,
                    cached_tokens: 0,
                    cache_creation_tokens: 0,
                    cached_tokens_unknown: false,
                    cache_creation_long_tokens: 0,
                    total_tokens: 65,
                };
                let final_chunk = InferenceResponseChunk {
                    delta: String::new(),
                    tool_calls: vec![],
                    usage: Some(usage),
                    error: None,
                    ..Default::default()
                };
                let _ = tx.send(Ok(final_chunk)).await;
            } else {
                // Default fallback response
                let text = "This is a mock LLM response stream.";
                for char_block in text.split_whitespace() {
                    let chunk = InferenceResponseChunk {
                        delta: format!("{} ", char_block),
                        tool_calls: vec![],
                        usage: None,
                        error: None,
                        ..Default::default()
                    };
                    let _ = tx.send(Ok(chunk)).await;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }

                let usage = TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 10,
                    cached_tokens: 0,
                    cache_creation_tokens: 0,
                    cached_tokens_unknown: false,
                    cache_creation_long_tokens: 0,
                    total_tokens: 20,
                };
                let final_chunk = InferenceResponseChunk {
                    delta: String::new(),
                    tool_calls: vec![],
                    usage: Some(usage),
                    error: None,
                    ..Default::default()
                };
                let _ = tx.send(Ok(final_chunk)).await;
            }
        });

        Ok(rx)
    }
}

// --- OPENAI PROVIDER IMPLEMENTATION ---

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    retry: RetryPolicy,
}

impl Default for OpenAiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAiProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            retry: RetryPolicy::default(),
        }
    }

    /// Replaces the retry policy (tests use a zero delay).
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn parse_sse_line(
        &self,
        line: &str,
    ) -> Option<Result<InferenceResponseChunk, VamInferenceError>> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }

        if !line.starts_with("data:") {
            return None;
        }

        let data = line["data:".len()..].trim();
        if data == "[DONE]" {
            return None;
        }

        #[derive(serde::Deserialize)]
        struct OpenAiChunk {
            choices: Vec<OpenAiChoice>,
            usage: Option<serde_json::Value>,
        }

        #[derive(serde::Deserialize)]
        struct OpenAiChoice {
            delta: OpenAiDelta,
        }

        #[derive(serde::Deserialize)]
        struct OpenAiDelta {
            content: Option<String>,
            reasoning_content: Option<String>,
            reasoning: Option<String>,
            tool_calls: Option<Vec<OpenAiToolCall>>,
        }

        #[derive(serde::Deserialize)]
        struct OpenAiToolCall {
            id: Option<String>,
            function: Option<OpenAiFunctionCall>,
        }

        #[derive(serde::Deserialize)]
        struct OpenAiFunctionCall {
            name: Option<String>,
            arguments: Option<String>,
        }

        match serde_json::from_str::<OpenAiChunk>(data) {
            Ok(chunk) => {
                let mut delta = String::new();
                let mut reasoning = String::new();
                let mut tool_calls = Vec::new();

                for choice in chunk.choices {
                    if let Some(r) = reasoning_text(
                        choice.delta.reasoning_content.as_deref(),
                        choice.delta.reasoning.as_deref(),
                        None,
                    ) {
                        reasoning.push_str(r);
                    }
                    if let Some(c) = choice.delta.content {
                        delta.push_str(&c);
                    }
                    if let Some(tcs) = choice.delta.tool_calls {
                        for tc in tcs {
                            if let (Some(id), Some(func)) = (tc.id, tc.function) {
                                tool_calls.push(ToolCall {
                                    id,
                                    name: func.name.unwrap_or_default(),
                                    arguments: func.arguments.unwrap_or_default(),
                                });
                            }
                        }
                    }
                }

                let usage = chunk
                    .usage
                    .filter(|u| u.is_object())
                    .map(|u| parse_openai_usage(&u));

                Some(Ok(InferenceResponseChunk {
                    delta,
                    tool_calls,
                    usage,
                    error: None,
                    reasoning,
                }))
            }
            Err(e) => Some(Err(VamInferenceError::Serialization(e.to_string()))),
        }
    }
}

#[async_trait]
impl InferenceProvider for OpenAiProvider {
    async fn infer(
        &self,
        request: InferenceRequest,
        catalog_entry: ModelCatalogEntry,
        api_key: Option<SecretString>,
        plan: &CachePlan,
    ) -> Result<mpsc::Receiver<Result<InferenceResponseChunk, VamInferenceError>>, VamInferenceError>
    {
        self.infer_with(
            request,
            catalog_entry,
            api_key,
            plan,
            &WireOptions::default(),
        )
        .await
    }

    async fn infer_with(
        &self,
        request: InferenceRequest,
        catalog_entry: ModelCatalogEntry,
        api_key: Option<SecretString>,
        plan: &CachePlan,
        wire: &WireOptions,
    ) -> Result<mpsc::Receiver<Result<InferenceResponseChunk, VamInferenceError>>, VamInferenceError>
    {
        let key = api_key.ok_or_else(|| VamInferenceError::SecretNotFound {
            key: catalog_entry.secret_ref.clone(),
        })?;

        let endpoint = catalog_entry.endpoint.clone();

        // OpenAI-compatible APIs reject '.' and names over 64 characters. Send a
        // wire name and map tool calls back to the guest name before they leave.
        let api_to_canonical = wire_to_canonical_names(&request.tools);
        let body = build_openai_body_with(&request, &catalog_entry, plan, wire);
        let fallback_body = (plan.mode != CacheMode::Implicit)
            .then(|| build_openai_body_with(&request, &catalog_entry, &plan.downgraded(), wire));
        let reasoning_field = catalog_entry
            .adapter
            .as_ref()
            .map(|a| a.reasoning_field.clone())
            .filter(|f| !f.is_empty());
        let cache_scope = (
            catalog_entry.provider.clone(),
            catalog_entry.model_name.clone(),
        );

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", key.expose_secret()))
                .map_err(|e| VamInferenceError::Internal(e.to_string()))?,
        );

        if endpoint.contains("openrouter.ai") {
            headers.insert(
                "HTTP-Referer",
                reqwest::header::HeaderValue::from_static("https://github.com/squrell-inc/VAM"),
            );
            headers.insert("X-Title", reqwest::header::HeaderValue::from_static("VAM"));
        }

        let (tx, rx) = mpsc::channel(32);
        let client_clone = self.client.clone();
        let retry = self.retry;

        tokio::spawn(async move {
            let mut sent = send_with_retry(retry, || {
                client_clone
                    .post(&endpoint)
                    .headers(headers.clone())
                    .json(&body)
            })
            .await;
            // A 400 that names a cache field: one retry without cache fields.
            if let (Ok(resp), Some(fallback)) = (sent.as_ref(), fallback_body.as_ref()) {
                if resp.status().as_u16() == 400 {
                    let text = match sent {
                        Ok(r) => r.text().await.unwrap_or_default(),
                        Err(_) => String::new(),
                    };
                    if is_cache_related_error(&text) {
                        sent = send_with_retry(retry, || {
                            client_clone
                                .post(&endpoint)
                                .headers(headers.clone())
                                .json(fallback)
                        })
                        .await;
                        // Remembered only when the cache-free retry is accepted:
                        // a 400 with another cause must not turn caching off.
                        if sent.as_ref().is_ok_and(|r| r.status().is_success()) {
                            note_downgrade(&cache_scope.0, &cache_scope.1, &text);
                        }
                    } else {
                        let _ = tx
                            .send(Err(VamInferenceError::ProviderError {
                                provider: "openai".to_string(),
                                details: format!("HTTP Status 400 Bad Request: {}", text),
                            }))
                            .await;
                        return;
                    }
                }
            }
            match sent {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        let status_code = resp.status();
                        let error_text = resp.text().await.unwrap_or_default();
                        let _ = tx
                            .send(Err(VamInferenceError::ProviderError {
                                provider: "openai".to_string(),
                                details: format!("HTTP Status {}: {}", status_code, error_text),
                            }))
                            .await;
                        return;
                    }

                    let mut bytes_stream = resp.bytes_stream();
                    let mut buffer = String::new();
                    let mut accumulated_tool_calls =
                        std::collections::BTreeMap::<u32, ToolCall>::new();
                    let mut total_usage = None;

                    while let Some(chunk_res) = bytes_stream.next().await {
                        match chunk_res {
                            Ok(chunk_data) => {
                                if let Ok(s) = std::str::from_utf8(&chunk_data) {
                                    buffer.push_str(s);
                                    while let Some(pos) = buffer.find('\n') {
                                        let line = buffer.drain(..=pos).collect::<String>();
                                        let line_trim = line.trim();
                                        if let Some(rest) = line_trim.strip_prefix("data:") {
                                            let data_part = rest.trim();
                                            if data_part == "[DONE]" {
                                                continue;
                                            }
                                            if let Ok(val) =
                                                serde_json::from_str::<serde_json::Value>(data_part)
                                            {
                                                // Extract choices
                                                if let Some(choices) =
                                                    val.get("choices").and_then(|v| v.as_array())
                                                {
                                                    for choice in choices {
                                                        // Extract delta
                                                        if let Some(delta) = choice.get("delta") {
                                                            // 0. Reasoning delta (host-only)
                                                            if let Some(r) = reasoning_text(
                                                                delta
                                                                    .get("reasoning_content")
                                                                    .and_then(|v| v.as_str()),
                                                                delta
                                                                    .get("reasoning")
                                                                    .and_then(|v| v.as_str()),
                                                                reasoning_field.as_deref(),
                                                            ) {
                                                                let _ = tx
                                                                    .send(Ok(
                                                                        InferenceResponseChunk {
                                                                            reasoning: r
                                                                                .to_string(),
                                                                            ..Default::default()
                                                                        },
                                                                    ))
                                                                    .await;
                                                            }
                                                            // 1. Text delta
                                                            if let Some(content) = delta
                                                                .get("content")
                                                                .and_then(|v| v.as_str())
                                                            {
                                                                if !content.is_empty() {
                                                                    let _ = tx.send(Ok(InferenceResponseChunk {
                                                                        delta: content.to_string(),
                                                                        tool_calls: vec![],
                                                                        usage: None,
                                                                        error: None,
                                                                        ..Default::default()
                                                                    })).await;
                                                                }
                                                            }
                                                            // 2. Tool calls delta
                                                            if let Some(tool_calls) = delta
                                                                .get("tool_calls")
                                                                .and_then(|v| v.as_array())
                                                            {
                                                                for tc_val in tool_calls {
                                                                    let idx = tc_val
                                                                        .get("index")
                                                                        .and_then(|v| v.as_u64())
                                                                        .unwrap_or(0)
                                                                        as u32;
                                                                    let id_opt = tc_val
                                                                        .get("id")
                                                                        .and_then(|v| v.as_str());
                                                                    let func_opt =
                                                                        tc_val.get("function");

                                                                    let entry =
                                                                        accumulated_tool_calls
                                                                            .entry(idx)
                                                                            .or_insert_with(|| {
                                                                                ToolCall {
                                                                                    id: String::new(
                                                                                    ),
                                                                                    name:
                                                                                        String::new(
                                                                                        ),
                                                                                    arguments:
                                                                                        String::new(
                                                                                        ),
                                                                                }
                                                                            });

                                                                    if let Some(id) = id_opt {
                                                                        entry.id = id.to_string();
                                                                    }

                                                                    if let Some(func) = func_opt {
                                                                        if let Some(name) = func
                                                                            .get("name")
                                                                            .and_then(|v| {
                                                                                v.as_str()
                                                                            })
                                                                        {
                                                                            entry.name = canonical_tool_name(
                                                                                &api_to_canonical,
                                                                                name,
                                                                            );
                                                                        }
                                                                        if let Some(args) = func
                                                                            .get("arguments")
                                                                            .and_then(|v| {
                                                                                v.as_str()
                                                                            })
                                                                        {
                                                                            entry
                                                                                .arguments
                                                                                .push_str(args);
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                // Extract usage
                                                if let Some(usage_val) =
                                                    val.get("usage").filter(|u| u.is_object())
                                                {
                                                    total_usage =
                                                        Some(parse_openai_usage(usage_val));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(Err(VamInferenceError::Internal(e.to_string())))
                                    .await;
                                return;
                            }
                        }
                    }

                    // After bytes_stream finishes, send any accumulated tool calls and final usage
                    let tcs: Vec<ToolCall> = accumulated_tool_calls.into_values().collect();
                    if !tcs.is_empty() || total_usage.is_some() {
                        let _ = tx
                            .send(Ok(InferenceResponseChunk {
                                delta: String::new(),
                                tool_calls: tcs,
                                usage: total_usage,
                                error: None,
                                ..Default::default()
                            }))
                            .await;
                    }
                }
                Err(e) => {
                    let _ = tx
                        .send(Err(VamInferenceError::Internal(e.to_string())))
                        .await;
                }
            }
        });

        Ok(rx)
    }
}

/// Usage of an OpenAI-format response (`usage` object).
///
/// `prompt_tokens` includes cached tokens. Cached tokens come from
/// `prompt_tokens_details.cached_tokens`; a cache-write count from
/// `prompt_tokens_details.cache_write_tokens`, `cache_creation_input_tokens`
/// or `cache_write_tokens` where a gateway returns one. A `usage` with no
/// `prompt_tokens_details` object (absent or null) and no top-level cache
/// field reports nothing about the cache: `cached_tokens_unknown` is set. A
/// present `cached_tokens: 0` is a real zero.
pub(crate) fn parse_openai_usage(usage: &serde_json::Value) -> TokenUsage {
    let int = |v: Option<&serde_json::Value>| -> Option<i32> {
        v.and_then(|v| v.as_i64())
            .map(|n| n.clamp(0, i32::MAX as i64) as i32)
    };
    let prompt = int(usage.get("prompt_tokens")).unwrap_or(0);
    let completion = int(usage.get("completion_tokens")).unwrap_or(0);
    let details = usage.get("prompt_tokens_details").filter(|d| d.is_object());
    let cached = int(details.and_then(|d| d.get("cached_tokens")))
        .or_else(|| int(usage.get("cache_read_input_tokens")));
    let written = int(details.and_then(|d| d.get("cache_write_tokens")))
        .or_else(|| int(usage.get("cache_creation_input_tokens")))
        .or_else(|| int(usage.get("cache_write_tokens")));
    let unknown = details.is_none() && cached.is_none() && written.is_none();
    let cached = cached.unwrap_or(0).min(prompt);
    let written = written.unwrap_or(0).min(prompt - cached);
    TokenUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        cached_tokens: cached,
        total_tokens: prompt + completion,
        cache_creation_tokens: written,
        cached_tokens_unknown: unknown,
        cache_creation_long_tokens: 0,
    }
}

/// A `cache_control` marker: the short TTL is the default and sends no `ttl`.
fn openai_part_marker(ttl: CacheTtl) -> serde_json::Value {
    match ttl {
        CacheTtl::Short => serde_json::json!({ "type": "ephemeral" }),
        CacheTtl::Long => serde_json::json!({ "type": "ephemeral", "ttl": "1h" }),
    }
}

/// Turns a message's string content into one text part carrying `marker`.
fn mark_openai_message(msg: &mut serde_json::Value, marker: serde_json::Value) {
    let text = msg["content"].as_str().unwrap_or_default().to_string();
    msg["content"] = serde_json::json!([{
        "type": "text",
        "text": text,
        "cache_control": marker,
    }]);
}

/// The chat-completions body of an OpenAI-compatible provider, with the
/// cache plan applied for the entry's mode:
/// * `PromptKey`: `prompt_cache_key`, and `prompt_cache_retention: "24h"` when
///   the entry's retention is long.
/// * `ExplicitBreakpoints` (OpenRouter, Anthropic models): per content-part
///   markers and no top-level directive (that would pin routing to direct
///   Anthropic). The static marker is on the last system message, the moving
///   marker on the last content part of the last non-`tool` message that has
///   text (in a tool loop: the assistant message before the trailing tool
///   results). At most two markers.
/// * `GatewayAuto` (Vercel): `providerOptions.gateway.caching = "auto"`.
/// * `Implicit`: no field.
#[cfg(test)]
pub(crate) fn build_openai_body(
    request: &InferenceRequest,
    entry: &ModelCatalogEntry,
    plan: &CachePlan,
) -> serde_json::Value {
    build_openai_body_with(request, entry, plan, &WireOptions::default())
}

/// The reasoning text of one delta: the entry's own field when it names one, otherwise
/// `reasoning_content`, then `reasoning` (never both, so a gateway that echoes both is
/// not counted twice). Empty strings are no reasoning.
fn reasoning_text<'a>(
    reasoning_content: Option<&'a str>,
    reasoning: Option<&'a str>,
    field: Option<&str>,
) -> Option<&'a str> {
    let picked = match field {
        Some("reasoning") => reasoning,
        Some("reasoning_content") => reasoning_content,
        _ => reasoning_content.filter(|r| !r.is_empty()).or(reasoning),
    };
    picked.filter(|r| !r.is_empty())
}

/// [`build_openai_body`] plus the run's harness options (`WireOptions::default()` adds
/// nothing, so the default body is byte-identical to the one without them):
/// * `reattach`: the remembered reasoning goes on its assistant message under the
///   entry's reasoning field (`keep`).
/// * `thinking_off`: the entry's fragment is merged into the top level.
/// * `tool_choice` and effort: set only when the plan carries them.
pub(crate) fn build_openai_body_with(
    request: &InferenceRequest,
    entry: &ModelCatalogEntry,
    plan: &CachePlan,
    wire: &WireOptions,
) -> serde_json::Value {
    let mut messages = Vec::new();
    for msg in &request.messages {
        let mut val_msg = serde_json::json!({
            "role": msg.role,
            "content": msg.content,
        });
        if msg.role == "assistant" && !msg.tool_calls.is_empty() {
            let tcs: Vec<serde_json::Value> = msg
                .tool_calls
                .iter()
                .map(|tc| {
                    serde_json::json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": provider_wire_tool_name(&tc.name),
                            "arguments": tc.arguments,
                        }
                    })
                })
                .collect();
            val_msg["tool_calls"] = serde_json::json!(tcs);
        } else if msg.role == "tool" {
            val_msg["tool_call_id"] = serde_json::json!(msg.tool_call_id);
        }
        messages.push(val_msg);
    }
    if let Some(field) = wire.plan.reasoning_field.as_deref() {
        for (idx, text) in &wire.reattach {
            if let Some(m) = messages.get_mut(*idx) {
                m[field] = serde_json::json!(text);
            }
        }
    }

    if plan.mode == CacheMode::ExplicitBreakpoints && entry.provider == "openrouter" {
        let has_text = |m: &serde_json::Value| m["content"].as_str().is_some_and(|t| !t.is_empty());
        let last_system = messages
            .iter()
            .rposition(|m| m["role"] == "system" && has_text(m));
        if let Some(i) = last_system {
            mark_openai_message(&mut messages[i], openai_part_marker(plan.static_ttl));
        }
        // A system message already turned into parts keeps its static marker.
        let moving = messages
            .iter()
            .rposition(|m| m["role"] != "tool" && has_text(m));
        if let Some(i) = moving {
            mark_openai_message(&mut messages[i], openai_part_marker(plan.moving_ttl));
        }
    }

    let mut body = serde_json::json!({
        "model": entry.model_name,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true }
    });

    if let Some(pref) = request.model_preference.as_ref() {
        if pref.temperature > 0.0 {
            body["temperature"] = serde_json::json!(pref.temperature);
        }
        if pref.max_completion_tokens > 0 {
            body["max_tokens"] = serde_json::json!(pref.max_completion_tokens);
        }
    }

    if !request.tools.is_empty() {
        let tools: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": provider_wire_tool_name(&tool.name),
                        "description": tool.description,
                        "parameters": tool_parameters_schema(&tool.input_schema_json),
                    }
                })
            })
            .collect();
        body["tools"] = serde_json::json!(tools);
        if let Some(choice) = wire.plan.tool_choice.as_deref() {
            body["tool_choice"] = serde_json::json!(choice);
        }
    }
    if let Some((field, value)) = wire.plan.effort.as_ref() {
        body[field.as_str()] = serde_json::json!(value);
    }
    if let Some(fragment) = wire.plan.thinking_off.as_ref() {
        for (k, v) in fragment {
            body[k.as_str()] = v.clone();
        }
    }

    match plan.mode {
        CacheMode::PromptKey => {
            if let Some(key) = &plan.key {
                body["prompt_cache_key"] = serde_json::json!(key.as_str());
            }
            if plan.retention == crate::cache::CacheRetention::Long {
                body["prompt_cache_retention"] = serde_json::json!("24h");
            }
        }
        CacheMode::GatewayAuto => {
            body["providerOptions"] = serde_json::json!({ "gateway": { "caching": "auto" } });
        }
        CacheMode::ExplicitBreakpoints | CacheMode::Implicit => {}
    }
    body
}

// --- ANTHROPIC PROVIDER IMPLEMENTATION ---

/// `max_tokens` for Anthropic when the model preference sets none. Streaming is
/// used, so a large ceiling carries no timeout risk.
const ANTHROPIC_DEFAULT_MAX_TOKENS: i64 = 16_000;

/// Marks a content block as the end of a cacheable prefix. The short TTL is
/// the API default (5 minutes) and sends no `ttl`; the long one is `1h`.
fn cache_marker(ttl: CacheTtl) -> serde_json::Value {
    openai_part_marker(ttl)
}

/// A tool result the guest reports as a failure.
fn tool_result_is_error(content: &str) -> bool {
    let head = content.trim_start();
    head.starts_with("Error:") || head.starts_with("tool_error:") || head.starts_with("error:")
}

/// The `/v1/messages` request body.
///
/// The cacheable prefix is `system` then `tools`: the last block of each
/// carries `cache_control`. The guest keeps both byte-identical between turns
/// (AR-0.7). The body never sets `temperature`, `top_p`, `tool_choice` or an
/// assistant prefill: the current Claude models reject all of them. Tool use
/// is left on the API default (`auto`).
fn build_anthropic_body(
    request: &InferenceRequest,
    model: &str,
    plan: &CachePlan,
) -> serde_json::Value {
    let explicit = plan.mode == CacheMode::ExplicitBreakpoints;
    let system_text = request
        .messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut messages: Vec<serde_json::Value> = Vec::new();
    fn push_blocks(
        messages: &mut Vec<serde_json::Value>,
        role: &str,
        blocks: Vec<serde_json::Value>,
    ) {
        if blocks.is_empty() {
            return;
        }
        if let Some(last) = messages.last_mut() {
            if last["role"] == role {
                if let Some(content) = last["content"].as_array_mut() {
                    content.extend(blocks);
                    return;
                }
            }
        }
        messages.push(serde_json::json!({ "role": role, "content": blocks }));
    }

    for msg in request.messages.iter().filter(|m| m.role != "system") {
        match msg.role.as_str() {
            "assistant" => {
                let mut blocks = Vec::new();
                if !msg.content.trim().is_empty() {
                    blocks.push(serde_json::json!({ "type": "text", "text": msg.content }));
                }
                for call in &msg.tool_calls {
                    let input = serde_json::from_str::<serde_json::Value>(&call.arguments)
                        .ok()
                        .filter(|v| v.is_object())
                        .unwrap_or_else(|| serde_json::json!({}));
                    blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": provider_wire_tool_name(&call.name),
                        "input": input,
                    }));
                }
                push_blocks(&mut messages, "assistant", blocks);
            }
            "tool" => {
                let mut block = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": msg.tool_call_id,
                    "content": msg.content,
                });
                if tool_result_is_error(&msg.content) {
                    block["is_error"] = serde_json::json!(true);
                }
                // Consecutive results join one user message, in call order.
                push_blocks(&mut messages, "user", vec![block]);
            }
            _ => {
                let blocks = if msg.content.trim().is_empty() {
                    vec![]
                } else {
                    vec![serde_json::json!({ "type": "text", "text": msg.content })]
                };
                push_blocks(&mut messages, "user", blocks);
            }
        }
    }

    let max_tokens = request
        .model_preference
        .as_ref()
        .map(|p| p.max_completion_tokens as i64)
        .filter(|n| *n > 0)
        .unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS);

    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "max_tokens": max_tokens,
    });
    if !system_text.is_empty() {
        body["system"] = serde_json::json!([{
            "type": "text",
            "text": system_text,
        }]);
        if explicit {
            // Tools render before system, so this marker caches both.
            body["system"][0]["cache_control"] = cache_marker(plan.static_ttl);
        }
    }
    if !request.tools.is_empty() {
        let mut tools: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": provider_wire_tool_name(&tool.name),
                    "description": tool.description,
                    "input_schema": tool_parameters_schema(&tool.input_schema_json),
                })
            })
            .collect();
        if explicit && system_text.is_empty() {
            // No system block: the static marker goes on the last tool.
            if let Some(last) = tools.last_mut() {
                last["cache_control"] = cache_marker(plan.static_ttl);
            }
        }
        body["tools"] = serde_json::json!(tools);
    }
    if explicit {
        // Moving breakpoint: the last block of the last message. The longer
        // TTL (static) comes first in render order, as the API requires.
        if let Some(block) = body["messages"]
            .as_array_mut()
            .and_then(|m| m.last_mut())
            .and_then(|m| m["content"].as_array_mut())
            .and_then(|c| c.last_mut())
        {
            block["cache_control"] = cache_marker(plan.moving_ttl);
        }
    }
    body
}

#[derive(Debug, Default, Clone, Copy)]
struct AnthropicUsageAcc {
    input: i32,
    cache_read: i32,
    cache_creation: i32,
    /// Part of `cache_creation` written with the 1 hour TTL.
    cache_creation_long: i32,
    output: i32,
    seen: bool,
    /// A cache field was present in a usage object.
    cache_fields_seen: bool,
}

impl AnthropicUsageAcc {
    fn absorb(&mut self, usage: &serde_json::Value) {
        let field = |name: &str| usage.get(name).and_then(|v| v.as_i64()).map(|n| n as i32);
        if let Some(n) = field("input_tokens") {
            self.input = n;
        }
        if let Some(n) = field("cache_read_input_tokens") {
            self.cache_read = n;
            self.cache_fields_seen = true;
        }
        if let Some(n) = field("cache_creation_input_tokens") {
            self.cache_creation = n;
            self.cache_fields_seen = true;
        }
        if let Some(n) = usage
            .get("cache_creation")
            .and_then(|c| c.get("ephemeral_1h_input_tokens"))
            .and_then(|v| v.as_i64())
        {
            self.cache_creation_long = n as i32;
        }
        if let Some(n) = field("output_tokens") {
            self.output = n;
        }
        self.seen = true;
    }

    fn to_usage(self) -> TokenUsage {
        let prompt = self.input + self.cache_read + self.cache_creation;
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: self.output,
            cached_tokens: self.cache_read,
            total_tokens: prompt + self.output,
            cache_creation_tokens: self.cache_creation,
            cached_tokens_unknown: !self.cache_fields_seen,
            cache_creation_long_tokens: self.cache_creation_long.min(self.cache_creation),
        }
    }
}

#[derive(Debug, Default)]
struct PendingToolUse {
    id: String,
    name: String,
    arguments: String,
}

/// Assembles one streamed Anthropic message: text deltas pass through, a
/// `tool_use` block is emitted once with its full arguments, and one usage
/// chunk follows at the end.
#[derive(Debug, Default)]
struct AnthropicStreamState {
    pending: std::collections::BTreeMap<u64, PendingToolUse>,
    usage: AnthropicUsageAcc,
}

impl AnthropicStreamState {
    fn tool_chunk(call: PendingToolUse) -> InferenceResponseChunk {
        let arguments = if call.arguments.trim().is_empty() {
            "{}".to_string()
        } else {
            call.arguments
        };
        InferenceResponseChunk {
            delta: String::new(),
            tool_calls: vec![ToolCall {
                id: call.id,
                name: call.name,
                arguments,
            }],
            usage: None,
            error: None,
            ..Default::default()
        }
    }

    fn handle(
        &mut self,
        event_type: &str,
        data: &str,
    ) -> Vec<Result<InferenceResponseChunk, VamInferenceError>> {
        let data = data.trim();
        if data.is_empty() {
            return vec![];
        }
        let value: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => return vec![Err(VamInferenceError::Serialization(e.to_string()))],
        };
        let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
        match event_type {
            "message_start" => {
                if let Some(usage) = value.get("message").and_then(|m| m.get("usage")) {
                    self.usage.absorb(usage);
                }
                vec![]
            }
            "content_block_start" => {
                let block = value.get("content_block");
                if block.and_then(|b| b.get("type")).and_then(|t| t.as_str()) == Some("tool_use") {
                    let text = |key: &str| {
                        block
                            .and_then(|b| b.get(key))
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string()
                    };
                    self.pending.insert(
                        index,
                        PendingToolUse {
                            id: text("id"),
                            name: text("name"),
                            arguments: String::new(),
                        },
                    );
                }
                vec![]
            }
            "content_block_delta" => {
                let delta = value.get("delta");
                match delta.and_then(|d| d.get("type")).and_then(|t| t.as_str()) {
                    Some("input_json_delta") => {
                        let part = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        if let Some(call) = self.pending.get_mut(&index) {
                            call.arguments.push_str(part);
                        }
                        vec![]
                    }
                    Some("text_delta") | None => {
                        let text = delta
                            .and_then(|d| d.get("text"))
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        if text.is_empty() {
                            return vec![];
                        }
                        vec![Ok(InferenceResponseChunk {
                            delta: text.to_string(),
                            tool_calls: vec![],
                            usage: None,
                            error: None,
                            ..Default::default()
                        })]
                    }
                    // Thinking and signature deltas are not part of Phase 0.
                    _ => vec![],
                }
            }
            "content_block_stop" => match self.pending.remove(&index) {
                Some(call) => vec![Ok(Self::tool_chunk(call))],
                None => vec![],
            },
            "message_delta" => {
                if let Some(usage) = value.get("usage") {
                    self.usage.absorb(usage);
                }
                vec![]
            }
            "error" => {
                let message = value
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("stream error");
                vec![Err(VamInferenceError::ProviderError {
                    provider: "anthropic".to_string(),
                    details: message.to_string(),
                })]
            }
            _ => vec![],
        }
    }

    /// Calls still open at the end of the stream, then the usage chunk.
    fn finish(&mut self) -> Vec<InferenceResponseChunk> {
        let mut out: Vec<InferenceResponseChunk> = std::mem::take(&mut self.pending)
            .into_values()
            .map(Self::tool_chunk)
            .collect();
        if self.usage.seen {
            out.push(InferenceResponseChunk {
                delta: String::new(),
                tool_calls: vec![],
                usage: Some(self.usage.to_usage()),
                error: None,
                ..Default::default()
            });
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    retry: RetryPolicy,
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            retry: RetryPolicy::default(),
        }
    }

    /// Replaces the retry policy (tests use a zero delay).
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Parses one SSE event on its own. Streaming uses [`AnthropicStreamState`],
    /// which also joins tool arguments and usage across events.
    pub fn parse_sse_line(
        &self,
        event_type: &str,
        data: &str,
    ) -> Option<Result<InferenceResponseChunk, VamInferenceError>> {
        AnthropicStreamState::default()
            .handle(event_type, data)
            .into_iter()
            .next()
    }
}

#[async_trait]
impl InferenceProvider for AnthropicProvider {
    async fn infer(
        &self,
        request: InferenceRequest,
        catalog_entry: ModelCatalogEntry,
        api_key: Option<SecretString>,
        plan: &CachePlan,
    ) -> Result<mpsc::Receiver<Result<InferenceResponseChunk, VamInferenceError>>, VamInferenceError>
    {
        let key = api_key.ok_or_else(|| VamInferenceError::SecretNotFound {
            key: catalog_entry.secret_ref.clone(),
        })?;

        let endpoint = catalog_entry.endpoint.clone();
        // Same wire alias as OpenAI: names over 64 characters are not sent raw.
        let wire_to_canonical = wire_to_canonical_names(&request.tools);
        let body = build_anthropic_body(&request, &catalog_entry.model_name, plan);
        let fallback_body = (plan.mode != CacheMode::Implicit)
            .then(|| build_anthropic_body(&request, &catalog_entry.model_name, &plan.downgraded()));
        let cache_scope = (
            catalog_entry.provider.clone(),
            catalog_entry.model_name.clone(),
        );

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-api-key",
            reqwest::header::HeaderValue::from_str(key.expose_secret())
                .map_err(|e| VamInferenceError::Internal(e.to_string()))?,
        );
        headers.insert(
            "anthropic-version",
            reqwest::header::HeaderValue::from_static("2023-06-01"),
        );

        let (tx, rx) = mpsc::channel(32);
        let client = self.client.clone();
        let retry = self.retry;

        tokio::spawn(async move {
            let sent = send_with_retry(retry, || {
                client.post(&endpoint).headers(headers.clone()).json(&body)
            })
            .await;
            let mut resp = match sent {
                Ok(resp) => resp,
                Err(e) => {
                    let message = e.to_string().replace(key.expose_secret(), "***");
                    let _ = tx.send(Err(VamInferenceError::Internal(message))).await;
                    return;
                }
            };
            // A 400 that names a cache field: one retry without cache fields.
            if let (400, Some(fallback)) = (resp.status().as_u16(), fallback_body.as_ref()) {
                let text = resp.text().await.unwrap_or_default();
                let text = text.replace(key.expose_secret(), "***");
                if !is_cache_related_error(&text) {
                    let _ = tx
                        .send(Err(VamInferenceError::ProviderError {
                            provider: "anthropic".to_string(),
                            details: format!("HTTP Status 400 Bad Request: {text}"),
                        }))
                        .await;
                    return;
                }
                resp = match send_with_retry(retry, || {
                    client
                        .post(&endpoint)
                        .headers(headers.clone())
                        .json(fallback)
                })
                .await
                {
                    Ok(resp) => resp,
                    Err(e) => {
                        let message = e.to_string().replace(key.expose_secret(), "***");
                        let _ = tx.send(Err(VamInferenceError::Internal(message))).await;
                        return;
                    }
                };
                // Remembered only when the cache-free retry is accepted.
                if resp.status().is_success() {
                    note_downgrade(&cache_scope.0, &cache_scope.1, &text);
                }
            }
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let text = text.replace(key.expose_secret(), "***");
                let _ = tx
                    .send(Err(VamInferenceError::ProviderError {
                        provider: "anthropic".to_string(),
                        details: format!("HTTP Status {status}: {text}"),
                    }))
                    .await;
                return;
            }

            let mut bytes_stream = resp.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();
            let mut current_event = String::new();
            let mut state = AnthropicStreamState::default();

            let canonical = |mut chunk: InferenceResponseChunk| {
                for call in &mut chunk.tool_calls {
                    call.name = canonical_tool_name(&wire_to_canonical, &call.name);
                }
                chunk
            };

            while let Some(chunk_res) = bytes_stream.next().await {
                let chunk_data = match chunk_res {
                    Ok(data) => data,
                    Err(e) => {
                        let _ = tx
                            .send(Err(VamInferenceError::Internal(e.to_string())))
                            .await;
                        return;
                    }
                };
                buffer.extend_from_slice(&chunk_data);
                while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                    let raw: Vec<u8> = buffer.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&raw);
                    let trimmed = line.trim();
                    if let Some(rest) = trimmed.strip_prefix("event:") {
                        current_event = rest.trim().to_string();
                    } else if let Some(rest) = trimmed.strip_prefix("data:") {
                        for res in state.handle(&current_event, rest) {
                            let res = res.map(&canonical);
                            if tx.send(res).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            for chunk in state.finish() {
                if tx.send(Ok(canonical(chunk))).await.is_err() {
                    return;
                }
            }
        });

        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::ChatMessage;

    #[tokio::test]
    async fn test_mock_provider_user_run_echo() {
        let provider = MockProvider::new();
        let request = InferenceRequest {
            model_preference: None,
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "please run-echo now".to_string(),
                tool_calls: vec![],
                tool_call_id: String::new(),
            }],
            tools: vec![],
            context_hint: None,
        };

        let catalog_entry = ModelCatalogEntry {
            provider: "mock".to_string(),
            model_name: "mock-model".to_string(),
            endpoint: "http://localhost/mock-infer".to_string(),
            secret_ref: "mock/api-key".to_string(),
            price_prompt_micro: 0,
            price_completion_micro: 0,
            context_window: 1_000_000,
            supports_tools: true,
            supports_structured_output: true,
            ..Default::default()
        };

        let mut rx = provider
            .infer(request, catalog_entry, None, &CachePlan::implicit())
            .await
            .unwrap();
        let chunk1 = rx.recv().await.unwrap().unwrap();
        assert_eq!(chunk1.tool_calls.len(), 1);
        assert_eq!(chunk1.tool_calls[0].name, "echo");

        let chunk2 = rx.recv().await.unwrap().unwrap();
        assert_eq!(chunk2.usage.unwrap().total_tokens, 40);
    }

    fn mock_request(messages: Vec<ChatMessage>) -> InferenceRequest {
        InferenceRequest {
            model_preference: None,
            messages,
            tools: vec![],
            context_hint: None,
        }
    }

    fn mock_catalog_entry() -> ModelCatalogEntry {
        ModelCatalogEntry {
            provider: "mock".to_string(),
            model_name: "mock-model".to_string(),
            endpoint: "http://localhost/mock-infer".to_string(),
            secret_ref: "mock/api-key".to_string(),
            price_prompt_micro: 0,
            price_completion_micro: 0,
            context_window: 1_000_000,
            supports_tools: true,
            supports_structured_output: true,
            ..Default::default()
        }
    }

    fn chat(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        }
    }

    #[tokio::test]
    async fn test_mock_provider_marker_selects_the_script_over_fixtures() {
        let script: crate::mock_script::MockScript = serde_json::from_str(
            r#"{"id": "greet", "rules": [{"when": {}, "reply": {"text": "scripted hello"}}]}"#,
        )
        .expect("script parses");
        let scripts =
            crate::mock_script::MockScripts::from_scripts(vec![script]).expect("scripts load");
        let provider = MockProvider::with_scripts(Arc::new(scripts));
        // The user message carries the run-echo fixture word; the marker must win.
        let request = mock_request(vec![
            chat("system", "You greet people. mock-script:greet"),
            chat("user", "please run-echo now"),
        ]);
        let mut rx = provider
            .infer(request, mock_catalog_entry(), None, &CachePlan::implicit())
            .await
            .unwrap();
        let text_chunk = rx.recv().await.unwrap().unwrap();
        assert_eq!(text_chunk.delta, "scripted hello");
        assert!(text_chunk.tool_calls.is_empty());
        let final_chunk = rx.recv().await.unwrap().unwrap();
        // 54 message bytes / 4 for the prompt, 14 reply bytes / 4 for the completion.
        assert_eq!(final_chunk.usage.unwrap().total_tokens, 13 + 3);
    }

    #[tokio::test]
    async fn mock_provider_reports_one_trace_per_scripted_request() {
        use crate::mock_script::MockTrace;
        let script: crate::mock_script::MockScript = serde_json::from_str(
            r#"{"id": "greet", "rules": [{"when": {"step": "a", "stepStart": true}, "reply": {"text": "hi"}}]}"#,
        )
        .expect("script parses");
        let scripts =
            crate::mock_script::MockScripts::from_scripts(vec![script]).expect("scripts load");
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let provider = MockProvider::with_scripts(Arc::new(scripts)).with_trace(Some(Arc::new(
            move |trace: &MockTrace| sink.lock().unwrap().push(trace.clone()),
        )));
        for messages in [
            vec![
                chat("system", "mock-script:greet"),
                chat("user", "e2e-step:a"),
            ],
            vec![
                chat("system", "mock-script:greet"),
                chat("user", "no label"),
            ],
            vec![chat("system", "mock-script:ghost"), chat("user", "x")],
            vec![chat("user", "plain unscripted")],
        ] {
            let mut rx = provider
                .infer(
                    mock_request(messages),
                    mock_catalog_entry(),
                    None,
                    &CachePlan::implicit(),
                )
                .await
                .unwrap();
            while rx.recv().await.is_some() {}
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert!(matches!(
            &seen[0],
            MockTrace::RuleMatched { script, rule: 0, step: Some(step) } if script == "greet" && step == "a"
        ));
        assert!(matches!(
            &seen[1],
            MockTrace::Unmatched {
                reason: "no_rule",
                ..
            }
        ));
        assert!(matches!(
            &seen[2],
            MockTrace::Unmatched {
                reason: "not_loaded",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn test_mock_provider_unloaded_marker_is_an_error_chunk() {
        let provider = MockProvider::new();
        let request = mock_request(vec![chat("system", "mock-script:ghost")]);
        let mut rx = provider
            .infer(request, mock_catalog_entry(), None, &CachePlan::implicit())
            .await
            .unwrap();
        let chunk = rx.recv().await.unwrap().unwrap();
        let error = chunk.error.expect("an error chunk");
        assert_eq!(error.message, "mock script ghost: not loaded");
        assert!(rx.recv().await.is_none());
    }

    #[test]
    fn test_openai_sse_parsing() {
        let provider = OpenAiProvider::new();
        let sse_line = r#"data: {"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hello world!"},"finish_reason":null}]}"#;
        let res = provider.parse_sse_line(sse_line).unwrap().unwrap();
        assert_eq!(res.delta, "Hello world!");
        assert!(res.tool_calls.is_empty());
        assert!(res.usage.is_none());
    }

    #[test]
    fn test_anthropic_sse_parsing() {
        let provider = AnthropicProvider::new();
        let event_type = "content_block_delta";
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello world!"}}"#;
        let res = provider.parse_sse_line(event_type, data).unwrap().unwrap();
        assert_eq!(res.delta, "Hello world!");
        assert!(res.tool_calls.is_empty());
        assert!(res.usage.is_none());
    }

    #[tokio::test]
    async fn test_openai_tool_call_accumulation() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let response = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n\
                data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"calc\"}}]}}]}\n\n\
                data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"a\\\"\"}}]}}]}\n\n\
                data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":1}\"}}]}}]}\n\n\
                data: [DONE]\n\n";
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        let provider = OpenAiProvider::new();
        let req = InferenceRequest {
            model_preference: None,
            messages: vec![],
            tools: vec![],
            context_hint: None,
        };
        let catalog_entry = ModelCatalogEntry {
            provider: "openai".to_string(),
            model_name: "test-model".to_string(),
            endpoint: format!("http://127.0.0.1:{}", port),
            secret_ref: "openai/api-key".to_string(),
            price_prompt_micro: 0,
            price_completion_micro: 0,
            context_window: 1000,
            supports_tools: true,
            supports_structured_output: true,
            ..Default::default()
        };

        let mut rx = provider
            .infer(
                req,
                catalog_entry,
                Some(SecretString::new("key".to_string())),
                &CachePlan::implicit(),
            )
            .await
            .unwrap();
        let mut chunks = Vec::new();
        while let Some(res) = rx.recv().await {
            chunks.push(res.unwrap());
        }

        // The tool calls should be accumulated and sent in the final chunk
        assert!(!chunks.is_empty());
        let final_chunk = chunks.last().unwrap();
        assert_eq!(final_chunk.tool_calls.len(), 1);
        assert_eq!(final_chunk.tool_calls[0].id, "call_1");
        assert_eq!(final_chunk.tool_calls[0].name, "calc");
        assert_eq!(final_chunk.tool_calls[0].arguments, "{\"a\":1}");
    }

    fn named_tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: "long tool".to_string(),
            input_schema_json: r#"{"type":"object"}"#.to_string(),
            ..Default::default()
        }
    }

    fn catalog(provider: &str, endpoint: &str) -> ModelCatalogEntry {
        ModelCatalogEntry {
            provider: provider.to_string(),
            model_name: "test-model".to_string(),
            endpoint: endpoint.to_string(),
            secret_ref: format!("{provider}/api-key"),
            price_prompt_micro: 0,
            price_completion_micro: 0,
            context_window: 1000,
            supports_tools: true,
            supports_structured_output: true,
            ..Default::default()
        }
    }

    async fn read_http_body(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end = loop {
            let n = socket.read(&mut tmp).await.unwrap();
            assert!(n > 0, "client closed before headers");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]);
        let mut content_length = 0usize;
        for line in headers.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap();
            }
        }
        while buf.len() < header_end + content_length {
            let n = socket.read(&mut tmp).await.unwrap();
            assert!(n > 0, "client closed before body");
            buf.extend_from_slice(&tmp[..n]);
        }
        buf[header_end..header_end + content_length].to_vec()
    }

    #[test]
    fn wire_alias_keeps_short_names_and_hashes_long_ones() {
        let short = "a".repeat(64);
        assert_eq!(provider_wire_tool_name(&short), short);
        assert_eq!(provider_wire_tool_name("echo.ping"), "echo_ping");

        let name65 = "vercel_oauth__create_sandboxes_sessions_by_session_id_snapshot_v2";
        assert_eq!(name65.len(), 65);
        let alias65 = provider_wire_tool_name(name65);
        assert!(alias65.len() <= PROVIDER_TOOL_NAME_MAX_LEN, "{alias65}");
        assert_ne!(alias65, name65);
        let hash65 = sha256_hex_prefix(name65.as_bytes(), WIRE_ALIAS_HASH_HEX_LEN);
        assert_eq!(
            alias65,
            format!("{}_{hash65}", &name65[..WIRE_ALIAS_PREFIX_MAX_LEN])
        );
        assert!(alias65
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'));

        let name128 = "b".repeat(128);
        let alias128 = provider_wire_tool_name(&name128);
        assert_eq!(alias128.len(), PROVIDER_TOOL_NAME_MAX_LEN);
        let hash128 = sha256_hex_prefix(name128.as_bytes(), WIRE_ALIAS_HASH_HEX_LEN);
        assert_eq!(
            alias128,
            format!("{}_{hash128}", "b".repeat(WIRE_ALIAS_PREFIX_MAX_LEN))
        );

        let dotted = format!("{}.{}", "c".repeat(40), "d".repeat(30));
        assert!(dotted.chars().count() > PROVIDER_TOOL_NAME_MAX_LEN);
        let alias = provider_wire_tool_name(&dotted);
        assert!(alias.len() <= PROVIDER_TOOL_NAME_MAX_LEN);
        assert!(!alias.contains('.'));
        assert!(alias.ends_with(&sha256_hex_prefix(
            dotted.as_bytes(),
            WIRE_ALIAS_HASH_HEX_LEN
        )));
    }

    #[tokio::test]
    async fn long_tool_names_round_trip_as_openai_aliases() {
        let names = vec![
            "vercel_oauth__create_sandboxes_sessions_by_session_id_snapshot_v2".to_string(),
            "n".repeat(128),
        ];
        let (body_tx, mut body_rx) = tokio::sync::mpsc::channel(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = listener.accept().await.unwrap();
            let body = read_http_body(&mut socket).await;
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let sent: Vec<String> = json["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap().to_string())
                .collect();
            body_tx.send(sent.clone()).await.unwrap();
            let calls = sent
                .iter()
                .enumerate()
                .map(|(index, name)| {
                    format!(
                        r#"{{"index":{index},"id":"call_{index}","function":{{"name":"{name}","arguments":"{{}}"}}}}"#
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
                 data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{calls}]}}}}]}}\n\n\
                 data: [DONE]\n\n"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let provider = OpenAiProvider::new();
        let req = InferenceRequest {
            model_preference: None,
            messages: vec![],
            tools: names.iter().map(|name| named_tool(name)).collect(),
            context_hint: None,
        };
        let mut rx = provider
            .infer(
                req,
                catalog("openai", &format!("http://127.0.0.1:{port}")),
                Some(SecretString::new("key".to_string())),
                &CachePlan::implicit(),
            )
            .await
            .unwrap();
        let mut returned = Vec::new();
        while let Some(chunk) = rx.recv().await {
            returned.extend(chunk.unwrap().tool_calls.into_iter().map(|call| call.name));
        }
        let sent = body_rx.recv().await.unwrap();
        assert_eq!(sent.len(), names.len());
        for (full, wire) in names.iter().zip(sent.iter()) {
            assert!(wire.chars().count() <= PROVIDER_TOOL_NAME_MAX_LEN, "{wire}");
            assert_ne!(wire, full);
            assert_eq!(wire, &provider_wire_tool_name(full));
        }
        assert_eq!(returned, names);
    }

    #[tokio::test]
    async fn long_tool_names_round_trip_as_anthropic_aliases() {
        let names = vec![
            "vercel_oauth__create_sandboxes_sessions_by_session_id_snapshot_v2".to_string(),
            "n".repeat(128),
        ];
        let (body_tx, mut body_rx) = tokio::sync::mpsc::channel(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = listener.accept().await.unwrap();
            let body = read_http_body(&mut socket).await;
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let sent: Vec<String> = json["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_string())
                .collect();
            body_tx.send(sent.clone()).await.unwrap();
            let mut response = String::from(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            for (index, name) in sent.iter().enumerate() {
                response.push_str(&format!(
                    "event: content_block_start\n\
                     data: {{\"type\":\"content_block_start\",\"index\":{index},\"content_block\":{{\"type\":\"tool_use\",\"id\":\"toolu_{index}\",\"name\":\"{name}\",\"input\":{{}}}}}}\n\n"
                ));
            }
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let provider = AnthropicProvider::new();
        let req = InferenceRequest {
            model_preference: None,
            messages: vec![],
            tools: names.iter().map(|name| named_tool(name)).collect(),
            context_hint: None,
        };
        let mut rx = provider
            .infer(
                req,
                catalog("anthropic", &format!("http://127.0.0.1:{port}")),
                Some(SecretString::new("key".to_string())),
                &CachePlan::implicit(),
            )
            .await
            .unwrap();
        let mut returned = Vec::new();
        while let Some(chunk) = rx.recv().await {
            returned.extend(chunk.unwrap().tool_calls.into_iter().map(|call| call.name));
        }
        let sent = body_rx.recv().await.unwrap();
        assert_eq!(sent.len(), names.len());
        for (full, wire) in names.iter().zip(sent.iter()) {
            assert!(wire.chars().count() <= PROVIDER_TOOL_NAME_MAX_LEN, "{wire}");
            assert_ne!(wire, full);
            assert_eq!(wire, &provider_wire_tool_name(full));
        }
        assert_eq!(returned, names);
    }

    // --- Anthropic adapter (AR-0.7) ---

    /// Serves one canned HTTP response per connection, in order, and sends each
    /// request body on the returned channel.
    async fn serve_sequence(
        responses: Vec<String>,
    ) -> (u16, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        let (body_tx, body_rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = read_http_body(&mut socket).await;
                let _ = body_tx.send(body);
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (port, body_rx)
    }

    fn http_response(status: &str, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn sse_text_reply(text: &str) -> String {
        let body = format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":5,\"output_tokens\":1}}}}}}\n\n\
             event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n\
             event: message_delta\ndata: {{\"type\":\"message_delta\",\"usage\":{{\"output_tokens\":3}}}}\n\n"
        );
        http_response("200 OK", "text/event-stream", &body)
    }

    fn fast_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::ZERO,
        }
    }

    fn msg(role: &str, content: &str) -> vam_spec::v1::ChatMessage {
        vam_spec::v1::ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        }
    }

    fn two_turn_request() -> InferenceRequest {
        let mut assistant = msg("assistant", "Checking.");
        assistant.tool_calls = vec![
            ToolCall {
                id: "toolu_1".to_string(),
                name: "echo.ping".to_string(),
                arguments: r#"{"a":1}"#.to_string(),
            },
            ToolCall {
                id: "toolu_2".to_string(),
                name: "read_file".to_string(),
                arguments: "not json".to_string(),
            },
        ];
        let mut first = msg("tool", "pong");
        first.tool_call_id = "toolu_1".to_string();
        let mut second = msg("tool", "Error: no such file");
        second.tool_call_id = "toolu_2".to_string();
        InferenceRequest {
            model_preference: Some(vam_spec::v1::ModelPreference {
                provider: "anthropic".to_string(),
                model_name: "claude-sonnet-5-5".to_string(),
                temperature: 0.1,
                top_p: 1.0,
                max_completion_tokens: 0,
            }),
            messages: vec![
                msg("system", "You are an agent."),
                msg("user", "ping please"),
                assistant,
                first,
                second,
                msg("user", "Continue."),
            ],
            tools: vec![named_tool("echo.ping"), named_tool("read_file")],
            context_hint: None,
        }
    }

    #[tokio::test]
    async fn anthropic_two_turn_tool_request_body() {
        let (port, mut bodies) = serve_sequence(vec![sse_text_reply("done")]).await;
        let provider = AnthropicProvider::new().with_retry_policy(fast_retry());
        let entry = ModelCatalogEntry {
            cache_mode: CacheMode::ExplicitBreakpoints,
            cache_retention: CacheRetention::Long,
            ..catalog("anthropic", &format!("http://127.0.0.1:{port}"))
        };
        let plan = CachePlan::for_entry(&entry, None);
        let mut rx = provider
            .infer(
                two_turn_request(),
                entry,
                Some(SecretString::new("key".to_string())),
                &plan,
            )
            .await
            .unwrap();
        let mut text = String::new();
        let mut usage = None;
        while let Some(chunk) = rx.recv().await {
            let chunk = chunk.unwrap();
            text.push_str(&chunk.delta);
            if chunk.usage.is_some() {
                usage = chunk.usage;
            }
        }
        assert_eq!(text, "done");
        let usage = usage.expect("one usage chunk");
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (5, 3));
        assert_eq!(usage.total_tokens, 8);

        let body: serde_json::Value =
            serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 16000);
        assert_eq!(body["system"][0]["text"], "You are an agent.");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "echo_ping");
        assert!(tools[0].get("cache_control").is_none());
        assert!(tools[1].get("cache_control").is_none());

        let messages = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = messages
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        let assistant = messages[1]["content"].as_array().unwrap();
        assert_eq!(assistant[0]["type"], "text");
        assert_eq!(assistant[1]["type"], "tool_use");
        assert_eq!(assistant[1]["id"], "toolu_1");
        assert_eq!(assistant[1]["name"], "echo_ping");
        assert_eq!(assistant[1]["input"]["a"], 1);
        assert_eq!(assistant[2]["input"], serde_json::json!({}));
        let results = messages[2]["content"].as_array().unwrap();
        assert_eq!(results[0]["type"], "tool_result");
        assert_eq!(results[0]["tool_use_id"], "toolu_1");
        assert!(results[0].get("is_error").is_none());
        assert_eq!(results[1]["tool_use_id"], "toolu_2");
        assert_eq!(results[1]["is_error"], true);
        assert_eq!(results[2]["type"], "text");

        // Corrections: no sampling parameters, no forced tool choice, no prefill.
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("tool_choice").is_none());
        assert_ne!(messages.last().unwrap()["role"], "assistant");
    }

    #[test]
    fn anthropic_body_honours_max_tokens_and_never_sets_sampling_or_tool_choice() {
        let mut request = two_turn_request();
        request.model_preference = Some(vam_spec::v1::ModelPreference {
            provider: "anthropic".to_string(),
            model_name: "claude-haiku-5-5".to_string(),
            temperature: 0.9,
            top_p: 0.5,
            max_completion_tokens: 4096,
        });
        let body = build_anthropic_body(&request, "claude-haiku-5-5", &CachePlan::implicit());
        assert_eq!(body["max_tokens"], 4096);
        for key in ["temperature", "top_p", "top_k", "tool_choice"] {
            assert!(body.get(key).is_none(), "{key} must not be sent");
        }
        let last_role = body["messages"].as_array().unwrap().last().unwrap()["role"].clone();
        assert_eq!(last_role, "user");
    }

    #[test]
    fn anthropic_body_is_identical_for_identical_requests() {
        let a = build_anthropic_body(&two_turn_request(), "m", &CachePlan::implicit()).to_string();
        let b = build_anthropic_body(&two_turn_request(), "m", &CachePlan::implicit()).to_string();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn anthropic_sse_tool_use_parses_arguments_and_usage() {
        let body = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":20,\"output_tokens\":1}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"On it.\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"echo_ping\",\"input\":{}}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"1}\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":42}}\n\n";
        let (port, _bodies) =
            serve_sequence(vec![http_response("200 OK", "text/event-stream", body)]).await;
        let provider = AnthropicProvider::new().with_retry_policy(fast_retry());
        let mut req = two_turn_request();
        req.tools = vec![named_tool("echo.ping")];
        let mut rx = provider
            .infer(
                req,
                catalog("anthropic", &format!("http://127.0.0.1:{port}")),
                Some(SecretString::new("key".to_string())),
                &CachePlan::implicit(),
            )
            .await
            .unwrap();
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut usages = Vec::new();
        while let Some(chunk) = rx.recv().await {
            let chunk = chunk.unwrap();
            text.push_str(&chunk.delta);
            calls.extend(chunk.tool_calls);
            usages.extend(chunk.usage);
        }
        assert_eq!(text, "On it.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_9");
        assert_eq!(calls[0].name, "echo.ping");
        assert_eq!(calls[0].arguments, r#"{"a":1}"#);
        assert_eq!(usages.len(), 1);
        let usage = &usages[0];
        assert_eq!(usage.prompt_tokens, 130);
        assert_eq!(usage.cached_tokens, 100);
        assert_eq!(usage.cache_creation_tokens, 20);
        assert_eq!(usage.completion_tokens, 42);
        assert_eq!(usage.total_tokens, 172);
    }

    #[tokio::test]
    async fn anthropic_401_body_never_shows_the_key() {
        let secret = "sk-ant-very-secret";
        let body = format!(
            "{{\"type\":\"error\",\"error\":{{\"type\":\"authentication_error\",\"message\":\"invalid x-api-key {secret}\"}}}}"
        );
        let (port, _bodies) = serve_sequence(vec![http_response(
            "401 Unauthorized",
            "application/json",
            &body,
        )])
        .await;
        let provider = AnthropicProvider::new().with_retry_policy(fast_retry());
        let mut rx = provider
            .infer(
                two_turn_request(),
                catalog("anthropic", &format!("http://127.0.0.1:{port}")),
                Some(SecretString::new(secret.to_string())),
                &CachePlan::implicit(),
            )
            .await
            .unwrap();
        let err = rx.recv().await.unwrap().unwrap_err().to_string();
        assert!(err.contains("401"), "{err}");
        assert!(err.contains("authentication_error"), "{err}");
        assert!(!err.contains(secret), "{err}");
    }

    #[tokio::test]
    async fn anthropic_529_then_200_succeeds() {
        let (port, mut bodies) = serve_sequence(vec![
            http_response("529 Overloaded", "application/json", "{\"type\":\"error\"}"),
            sse_text_reply("ok"),
        ])
        .await;
        let provider = AnthropicProvider::new().with_retry_policy(fast_retry());
        let mut rx = provider
            .infer(
                two_turn_request(),
                catalog("anthropic", &format!("http://127.0.0.1:{port}")),
                Some(SecretString::new("key".to_string())),
                &CachePlan::implicit(),
            )
            .await
            .unwrap();
        let mut text = String::new();
        while let Some(chunk) = rx.recv().await {
            text.push_str(&chunk.unwrap().delta);
        }
        assert_eq!(text, "ok");
        bodies.recv().await.unwrap();
        bodies.recv().await.unwrap();
    }

    #[tokio::test]
    async fn anthropic_gives_up_after_the_bounded_retries() {
        let overloaded =
            || http_response("529 Overloaded", "application/json", "{\"error\":\"busy\"}");
        let (port, _bodies) = serve_sequence(vec![overloaded(), overloaded(), overloaded()]).await;
        let provider = AnthropicProvider::new().with_retry_policy(fast_retry());
        let mut rx = provider
            .infer(
                two_turn_request(),
                catalog("anthropic", &format!("http://127.0.0.1:{port}")),
                Some(SecretString::new("key".to_string())),
                &CachePlan::implicit(),
            )
            .await
            .unwrap();
        let err = rx.recv().await.unwrap().unwrap_err().to_string();
        assert!(err.contains("529") && err.contains("busy"), "{err}");
    }

    #[tokio::test]
    async fn openai_503_then_200_succeeds() {
        let ok = http_response(
            "200 OK",
            "text/event-stream",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
        );
        let (port, _bodies) = serve_sequence(vec![
            http_response("503 Service Unavailable", "text/plain", "later"),
            ok,
        ])
        .await;
        let provider = OpenAiProvider::new().with_retry_policy(fast_retry());
        let mut rx = provider
            .infer(
                InferenceRequest {
                    model_preference: None,
                    messages: vec![msg("user", "hi")],
                    tools: vec![],
                    context_hint: None,
                },
                catalog("openai", &format!("http://127.0.0.1:{port}")),
                Some(SecretString::new("key".to_string())),
                &CachePlan::implicit(),
            )
            .await
            .unwrap();
        let mut text = String::new();
        while let Some(chunk) = rx.recv().await {
            text.push_str(&chunk.unwrap().delta);
        }
        assert_eq!(text, "hi");
    }

    // --- prompt caching (AR-2.11): rendered bodies, usage fixtures, downgrade ---

    use crate::cache::{CacheKey, CacheMode, CachePlan, CacheRetention, CacheTtl};

    fn entry_with(provider: &str, mode: CacheMode, retention: CacheRetention) -> ModelCatalogEntry {
        ModelCatalogEntry {
            cache_mode: mode,
            cache_retention: retention,
            ..catalog(provider, "http://127.0.0.1:1")
        }
    }

    fn plan_for(entry: &ModelCatalogEntry) -> CachePlan {
        CachePlan::for_entry(entry, Some(CacheKey::derive("owner", "digest", "thread")))
    }

    fn count_markers(v: &serde_json::Value) -> usize {
        v.to_string().matches("cache_control").count()
    }

    #[test]
    fn anthropic_explicit_plan_puts_a_long_static_and_a_short_moving_marker() {
        let entry = entry_with(
            "anthropic",
            CacheMode::ExplicitBreakpoints,
            CacheRetention::Long,
        );
        let body = build_anthropic_body(&two_turn_request(), "m", &plan_for(&entry));
        // Static: the last system block, 1 hour. Tools carry none (system covers them).
        assert_eq!(
            body["system"][0]["cache_control"],
            serde_json::json!({ "type": "ephemeral", "ttl": "1h" })
        );
        for tool in body["tools"].as_array().unwrap() {
            assert!(tool.get("cache_control").is_none());
        }
        // Moving: the last block of the last message, 5 minutes (no ttl field).
        let messages = body["messages"].as_array().unwrap();
        let last_blocks = messages.last().unwrap()["content"].as_array().unwrap();
        assert_eq!(
            last_blocks.last().unwrap()["cache_control"],
            serde_json::json!({ "type": "ephemeral" })
        );
        assert_eq!(count_markers(&body), 2);
        // TTL order: the API renders tools, system, then messages, so the 1h
        // marker (system) comes before the 5m one (messages). The only 1h
        // marker is in `system`; none is in `messages` or `tools`.
        assert!(body["messages"].to_string().find("\"ttl\"").is_none());
        assert!(body["tools"].to_string().find("\"ttl\"").is_none());
    }

    #[test]
    fn anthropic_short_retention_sends_no_ttl_and_no_system_marks_the_last_tool() {
        let entry = entry_with(
            "anthropic",
            CacheMode::ExplicitBreakpoints,
            CacheRetention::Short,
        );
        let mut request = two_turn_request();
        let body = build_anthropic_body(&request, "m", &plan_for(&entry));
        assert!(!body.to_string().contains("\"ttl\""));
        request.messages.remove(0);
        let body = build_anthropic_body(&request, "m", &plan_for(&entry));
        assert!(body.get("system").is_none());
        let tools = body["tools"].as_array().unwrap();
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
        assert_eq!(count_markers(&body), 2);
    }

    #[test]
    fn anthropic_implicit_adds_no_fields_and_the_guest_hint_changes_nothing() {
        let entry = entry_with("anthropic", CacheMode::Implicit, CacheRetention::Long);
        let body = build_anthropic_body(&two_turn_request(), "m", &plan_for(&entry));
        assert_eq!(count_markers(&body), 0);

        let explicit = entry_with(
            "anthropic",
            CacheMode::ExplicitBreakpoints,
            CacheRetention::Long,
        );
        let plain = two_turn_request();
        let mut hinted = two_turn_request();
        hinted.context_hint = Some(vam_spec::v1::ContextHint {
            lane: vam_spec::v1::context_hint::Lane::Summary as i32,
            keep_from_index: 2,
            keep_from_msg_sha256: "x".into(),
            reason: "tokens".into(),
        });
        assert_eq!(
            build_anthropic_body(&plain, "m", &plan_for(&explicit)),
            build_anthropic_body(&hinted, "m", &plan_for(&explicit))
        );
    }

    #[test]
    fn openai_prompt_key_sends_key_and_24h_only_where_the_entry_allows_it() {
        let request = two_turn_request();
        let short = entry_with("openai", CacheMode::PromptKey, CacheRetention::Short);
        let body = build_openai_body(&request, &short, &plan_for(&short));
        let key = CacheKey::derive("owner", "digest", "thread");
        assert_eq!(body["prompt_cache_key"], key.as_str());
        assert!(body.get("prompt_cache_retention").is_none());

        let long = entry_with("openai", CacheMode::PromptKey, CacheRetention::Long);
        let body = build_openai_body(&request, &long, &plan_for(&long));
        assert_eq!(body["prompt_cache_retention"], "24h");

        // No host key: no key field.
        let no_key = CachePlan::for_entry(&short, None);
        let body = build_openai_body(&request, &short, &no_key);
        assert!(body.get("prompt_cache_key").is_none());
        // Messages stay plain strings for OpenAI.
        assert!(body["messages"][0]["content"].is_string());
    }

    #[test]
    fn openai_compatible_implicit_adds_no_fields() {
        for provider in ["openai", "openrouter", "vercel", "ollama"] {
            let entry = entry_with(provider, CacheMode::Implicit, CacheRetention::Long);
            let body = build_openai_body(&two_turn_request(), &entry, &plan_for(&entry));
            let text = body.to_string();
            for field in [
                "cache_control",
                "prompt_cache",
                "providerOptions",
                "caching",
            ] {
                assert!(!text.contains(field), "{provider}: {field}");
            }
        }
    }

    #[test]
    fn vercel_gateway_auto_sets_provider_options() {
        let entry = entry_with("vercel", CacheMode::GatewayAuto, CacheRetention::Short);
        let body = build_openai_body(&two_turn_request(), &entry, &plan_for(&entry));
        assert_eq!(
            body["providerOptions"],
            serde_json::json!({ "gateway": { "caching": "auto" } })
        );
        assert!(body.get("prompt_cache_key").is_none());
        assert_eq!(count_markers(&body), 0);
    }

    fn openrouter_entry(retention: CacheRetention) -> ModelCatalogEntry {
        entry_with("openrouter", CacheMode::ExplicitBreakpoints, retention)
    }

    fn marked_parts(body: &serde_json::Value) -> Vec<(usize, serde_json::Value)> {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                let part = m["content"].as_array()?.last()?;
                part.get("cache_control").map(|c| (i, c.clone()))
            })
            .collect()
    }

    #[test]
    fn openrouter_markers_when_the_last_message_is_a_tool_result() {
        // two_turn_request ends with a user message; make it end with tool results.
        let mut request = two_turn_request();
        request.messages.pop();
        let entry = openrouter_entry(CacheRetention::Short);
        let body = build_openai_body(&request, &entry, &plan_for(&entry));
        let marked = marked_parts(&body);
        // System message (0) and the assistant message with the tool calls (2);
        // the trailing tool results (3, 4) carry nothing.
        assert_eq!(marked.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0, 2]);
        assert_eq!(body["messages"][4]["role"], "tool");
        assert!(body["messages"][4]["content"].is_string());
        assert_eq!(body["messages"][2]["content"][0]["text"], "Checking.");
        assert_eq!(
            body["messages"][2]["tool_calls"].as_array().unwrap().len(),
            2
        );
        // No top-level directive: routing stays open.
        assert!(body.get("cache_control").is_none());
        assert!(marked
            .iter()
            .all(|(_, c)| c == &serde_json::json!({ "type": "ephemeral" })));
    }

    #[test]
    fn openrouter_markers_when_the_last_message_is_not_a_tool_result() {
        let entry = openrouter_entry(CacheRetention::Short);
        let body = build_openai_body(&two_turn_request(), &entry, &plan_for(&entry));
        let marked = marked_parts(&body);
        // System (0) and the last user message (5).
        assert_eq!(marked.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0, 5]);
        assert!(count_markers(&body) <= 4);
    }

    #[test]
    fn openrouter_long_retention_marks_the_static_part_with_a_1h_ttl() {
        let entry = openrouter_entry(CacheRetention::Long);
        let body = build_openai_body(&two_turn_request(), &entry, &plan_for(&entry));
        let marked = marked_parts(&body);
        assert_eq!(marked[0].1["ttl"], "1h");
        assert!(marked[1].1.get("ttl").is_none());
    }

    #[test]
    fn openrouter_skips_an_assistant_message_with_no_text() {
        let mut request = two_turn_request();
        request.messages.pop();
        request.messages[2].content = String::new();
        let entry = openrouter_entry(CacheRetention::Short);
        let body = build_openai_body(&request, &entry, &plan_for(&entry));
        // The assistant message has no text part to mark: the user message before it is.
        assert_eq!(
            marked_parts(&body)
                .iter()
                .map(|(i, _)| *i)
                .collect::<Vec<_>>(),
            [0, 1]
        );
    }

    #[test]
    fn explicit_mode_changes_nothing_on_a_plain_openai_endpoint() {
        let entry = entry_with(
            "openai",
            CacheMode::ExplicitBreakpoints,
            CacheRetention::Short,
        );
        let body = build_openai_body(&two_turn_request(), &entry, &plan_for(&entry));
        assert_eq!(count_markers(&body), 0);
    }

    #[test]
    fn plan_ttls_follow_the_entry() {
        let long = entry_with(
            "anthropic",
            CacheMode::ExplicitBreakpoints,
            CacheRetention::Long,
        );
        let plan = plan_for(&long);
        assert_eq!(
            (plan.static_ttl, plan.moving_ttl),
            (CacheTtl::Long, CacheTtl::Short)
        );
    }

    // Usage fixtures: SSE as each provider returns it.

    async fn openai_usage_from(sse: &str) -> TokenUsage {
        let reply = http_response("200 OK", "text/event-stream", sse);
        let (port, _bodies) = serve_sequence(vec![reply]).await;
        let provider = OpenAiProvider::new().with_retry_policy(fast_retry());
        let mut rx = provider
            .infer(
                two_turn_request(),
                catalog("openai", &format!("http://127.0.0.1:{port}")),
                Some(SecretString::new("key".to_string())),
                &CachePlan::implicit(),
            )
            .await
            .unwrap();
        let mut usage = None;
        while let Some(chunk) = rx.recv().await {
            if let Some(u) = chunk.unwrap().usage {
                usage = Some(u);
            }
        }
        usage.expect("usage")
    }

    fn openai_sse(usage: &str) -> String {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"hi\"}}}}]}}\n\n\
data: {{\"choices\":[],\"usage\":{usage}}}\n\n\
data: [DONE]\n\n"
        )
    }

    #[tokio::test]
    async fn openai_usage_reports_cached_tokens() {
        let usage = openai_usage_from(&openai_sse(
            r#"{"prompt_tokens":2006,"completion_tokens":300,"total_tokens":2306,"prompt_tokens_details":{"cached_tokens":1920}}"#,
        ))
        .await;
        assert_eq!(usage.prompt_tokens, 2006);
        assert_eq!(usage.cached_tokens, 1920);
        assert_eq!(usage.cache_creation_tokens, 0);
        assert!(!usage.cached_tokens_unknown);
    }

    #[tokio::test]
    async fn openai_format_gateway_with_a_cache_write_field() {
        let usage = openai_usage_from(&openai_sse(
            r#"{"prompt_tokens":3000,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":1000,"cache_write_tokens":500}}"#,
        ))
        .await;
        assert_eq!(
            (usage.cached_tokens, usage.cache_creation_tokens),
            (1000, 500)
        );
        assert!(!usage.cached_tokens_unknown);
    }

    #[tokio::test]
    async fn a_gateway_reporting_cached_zero_is_zero_not_unknown() {
        let usage = openai_usage_from(&openai_sse(
            r#"{"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":0}}"#,
        ))
        .await;
        assert_eq!(usage.cached_tokens, 0);
        assert!(!usage.cached_tokens_unknown);
    }

    #[tokio::test]
    async fn absent_or_null_prompt_tokens_details_is_unknown() {
        for usage in [
            r#"{"prompt_tokens":100,"completion_tokens":5}"#,
            r#"{"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":null}"#,
        ] {
            let parsed = openai_usage_from(&openai_sse(usage)).await;
            assert_eq!(parsed.cached_tokens, 0);
            assert!(parsed.cached_tokens_unknown, "{usage}");
        }
    }

    #[test]
    fn parse_sse_line_shares_the_usage_parser() {
        let p = OpenAiProvider::new();
        let line = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":50,\"completion_tokens\":2,\"prompt_tokens_details\":{\"cached_tokens\":40}}}";
        let chunk = p.parse_sse_line(line).unwrap().unwrap();
        let usage = chunk.usage.unwrap();
        assert_eq!((usage.prompt_tokens, usage.cached_tokens), (50, 40));
        assert!(!usage.cached_tokens_unknown);
    }

    #[tokio::test]
    async fn anthropic_usage_splits_the_one_hour_write() {
        let sse = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":30,\"cache_creation\":{\"ephemeral_5m_input_tokens\":10,\"ephemeral_1h_input_tokens\":20},\"output_tokens\":1}}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";
        let reply = http_response("200 OK", "text/event-stream", sse);
        let (port, _bodies) = serve_sequence(vec![reply]).await;
        let provider = AnthropicProvider::new().with_retry_policy(fast_retry());
        let mut rx = provider
            .infer(
                two_turn_request(),
                catalog("anthropic", &format!("http://127.0.0.1:{port}")),
                Some(SecretString::new("key".to_string())),
                &CachePlan::implicit(),
            )
            .await
            .unwrap();
        let mut usage = None;
        while let Some(chunk) = rx.recv().await {
            if let Some(u) = chunk.unwrap().usage {
                usage = Some(u);
            }
        }
        let usage = usage.unwrap();
        assert_eq!(usage.prompt_tokens, 140);
        assert_eq!(usage.cached_tokens, 100);
        assert_eq!(usage.cache_creation_tokens, 30);
        assert_eq!(usage.cache_creation_long_tokens, 20);
        assert!(!usage.cached_tokens_unknown);
    }

    #[tokio::test]
    async fn a_cache_related_400_is_retried_once_without_cache_fields() {
        let mut entry = entry_with("openai", CacheMode::PromptKey, CacheRetention::Long);
        entry.model_name = "downgrade-test-model".to_string();
        let first = http_response(
            "400 Bad Request",
            "application/json",
            r#"{"error":{"message":"Unrecognized request argument: prompt_cache_retention"}}"#,
        );
        let second = http_response(
            "200 OK",
            "text/event-stream",
            &openai_sse(r#"{"prompt_tokens":5,"completion_tokens":1}"#),
        );
        let (port, mut bodies) = serve_sequence(vec![first, second]).await;
        entry.endpoint = format!("http://127.0.0.1:{port}");
        let plan = plan_for(&entry);
        let provider = OpenAiProvider::new().with_retry_policy(fast_retry());
        let mut rx = provider
            .infer(
                two_turn_request(),
                entry,
                Some(SecretString::new("key".to_string())),
                &plan,
            )
            .await
            .unwrap();
        let mut got_usage = false;
        while let Some(chunk) = rx.recv().await {
            got_usage |= chunk.unwrap().usage.is_some();
        }
        assert!(got_usage);
        let first_body: serde_json::Value =
            serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
        let second_body: serde_json::Value =
            serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
        assert!(first_body.get("prompt_cache_key").is_some());
        assert!(second_body.get("prompt_cache_key").is_none());
        assert!(second_body.get("prompt_cache_retention").is_none());
        assert!(crate::cache::is_downgraded(
            "openai",
            "downgrade-test-model"
        ));
    }

    #[tokio::test]
    async fn an_unrelated_400_is_not_retried() {
        let mut entry = entry_with("openai", CacheMode::PromptKey, CacheRetention::Short);
        entry.model_name = "no-downgrade-test-model".to_string();
        let only = http_response(
            "400 Bad Request",
            "application/json",
            r#"{"error":"invalid model"}"#,
        );
        let (port, _bodies) = serve_sequence(vec![only]).await;
        entry.endpoint = format!("http://127.0.0.1:{port}");
        let plan = plan_for(&entry);
        let provider = OpenAiProvider::new().with_retry_policy(fast_retry());
        let mut rx = provider
            .infer(
                two_turn_request(),
                entry,
                Some(SecretString::new("key".to_string())),
                &plan,
            )
            .await
            .unwrap();
        let first = rx.recv().await.unwrap();
        assert!(matches!(
            first,
            Err(VamInferenceError::ProviderError { .. })
        ));
        assert!(!crate::cache::is_downgraded(
            "openai",
            "no-downgrade-test-model"
        ));
    }

    fn sse_ok() -> String {
        http_response(
            "200 OK",
            "text/event-stream",
            &openai_sse(r#"{"prompt_tokens":5,"completion_tokens":1}"#),
        )
    }

    fn cache_400() -> String {
        http_response(
            "400 Bad Request",
            "application/json",
            r#"{"error":{"message":"Unrecognized request argument: prompt_cache_retention"}}"#,
        )
    }

    async fn drain(
        rx: &mut mpsc::Receiver<Result<InferenceResponseChunk, VamInferenceError>>,
    ) -> bool {
        let mut ok = true;
        while let Some(chunk) = rx.recv().await {
            ok &= chunk.is_ok();
        }
        ok
    }

    #[tokio::test]
    async fn downgrade_is_remembered_only_after_the_cache_free_retry_succeeds_openai() {
        let mut entry = entry_with("openai", CacheMode::PromptKey, CacheRetention::Long);
        entry.model_name = "downgrade-ok-openai".to_string();
        let (port, mut bodies) = serve_sequence(vec![cache_400(), sse_ok()]).await;
        entry.endpoint = format!("http://127.0.0.1:{port}");
        let plan = plan_for(&entry);
        let provider = OpenAiProvider::new().with_retry_policy(fast_retry());
        let mut rx = provider
            .infer(
                two_turn_request(),
                entry,
                Some(SecretString::new("key".to_string())),
                &plan,
            )
            .await
            .unwrap();
        assert!(drain(&mut rx).await);
        let _first = bodies.recv().await.unwrap();
        let second: serde_json::Value =
            serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
        assert!(second.get("prompt_cache_key").is_none());
        assert!(crate::cache::is_downgraded("openai", "downgrade-ok-openai"));
    }

    #[tokio::test]
    async fn a_failed_cache_free_retry_remembers_nothing_openai() {
        let mut entry = entry_with("openai", CacheMode::PromptKey, CacheRetention::Long);
        entry.model_name = "downgrade-fail-openai".to_string();
        let other = http_response(
            "400 Bad Request",
            "application/json",
            r#"{"error":"quota"}"#,
        );
        let (port, _bodies) = serve_sequence(vec![cache_400(), other]).await;
        entry.endpoint = format!("http://127.0.0.1:{port}");
        let plan = plan_for(&entry);
        let provider = OpenAiProvider::new().with_retry_policy(fast_retry());
        let mut rx = provider
            .infer(
                two_turn_request(),
                entry,
                Some(SecretString::new("key".to_string())),
                &plan,
            )
            .await
            .unwrap();
        let first = rx.recv().await.unwrap();
        assert!(matches!(
            first,
            Err(VamInferenceError::ProviderError { .. })
        ));
        assert!(!crate::cache::is_downgraded(
            "openai",
            "downgrade-fail-openai"
        ));
    }

    fn anthropic_ok() -> String {
        http_response(
            "200 OK",
            "text/event-stream",
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        )
    }

    fn anthropic_cache_400() -> String {
        http_response(
            "400 Bad Request",
            "application/json",
            r#"{"error":{"message":"cache_control.ttl: invalid"}}"#,
        )
    }

    #[tokio::test]
    async fn downgrade_is_remembered_only_after_the_cache_free_retry_succeeds_anthropic() {
        let mut entry = entry_with(
            "anthropic",
            CacheMode::ExplicitBreakpoints,
            CacheRetention::Long,
        );
        entry.model_name = "downgrade-ok-anthropic".to_string();
        let (port, mut bodies) = serve_sequence(vec![anthropic_cache_400(), anthropic_ok()]).await;
        entry.endpoint = format!("http://127.0.0.1:{port}");
        let plan = plan_for(&entry);
        let provider = AnthropicProvider::new().with_retry_policy(fast_retry());
        let mut rx = provider
            .infer(
                two_turn_request(),
                entry,
                Some(SecretString::new("key".to_string())),
                &plan,
            )
            .await
            .unwrap();
        assert!(drain(&mut rx).await);
        let first: serde_json::Value =
            serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
        let second: serde_json::Value =
            serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
        assert!(count_markers(&first) > 0);
        assert_eq!(count_markers(&second), 0);
        assert!(crate::cache::is_downgraded(
            "anthropic",
            "downgrade-ok-anthropic"
        ));
    }

    #[tokio::test]
    async fn a_failed_cache_free_retry_remembers_nothing_anthropic() {
        let mut entry = entry_with(
            "anthropic",
            CacheMode::ExplicitBreakpoints,
            CacheRetention::Long,
        );
        entry.model_name = "downgrade-fail-anthropic".to_string();
        let other = http_response("400 Bad Request", "application/json", r#"{"error":"bad"}"#);
        let (port, _bodies) = serve_sequence(vec![anthropic_cache_400(), other]).await;
        entry.endpoint = format!("http://127.0.0.1:{port}");
        let plan = plan_for(&entry);
        let provider = AnthropicProvider::new().with_retry_policy(fast_retry());
        let mut rx = provider
            .infer(
                two_turn_request(),
                entry,
                Some(SecretString::new("key".to_string())),
                &plan,
            )
            .await
            .unwrap();
        let first = rx.recv().await.unwrap();
        assert!(matches!(
            first,
            Err(VamInferenceError::ProviderError { .. })
        ));
        assert!(!crate::cache::is_downgraded(
            "anthropic",
            "downgrade-fail-anthropic"
        ));
    }

    #[tokio::test]
    async fn the_next_request_after_a_remembered_downgrade_sends_no_cache_fields() {
        let mut entry = entry_with("openai", CacheMode::PromptKey, CacheRetention::Long);
        entry.model_name = "downgrade-next-request".to_string();
        crate::cache::note_downgrade("openai", &entry.model_name, "test");
        // The broker builds the plan this way for every request.
        let plan = if crate::cache::is_downgraded(&entry.provider, &entry.model_name) {
            CachePlan::implicit()
        } else {
            plan_for(&entry)
        };
        let (port, mut bodies) = serve_sequence(vec![sse_ok()]).await;
        entry.endpoint = format!("http://127.0.0.1:{port}");
        let provider = OpenAiProvider::new().with_retry_policy(fast_retry());
        let mut rx = provider
            .infer(
                two_turn_request(),
                entry,
                Some(SecretString::new("key".to_string())),
                &plan,
            )
            .await
            .unwrap();
        assert!(drain(&mut rx).await);
        let body: serde_json::Value =
            serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("prompt_cache_retention").is_none());
    }

    // ---- reasoning and harness options (H-1.4, H-1.5) ----

    fn reasoning_entry(endpoint: &str, field: &str) -> ModelCatalogEntry {
        ModelCatalogEntry {
            adapter: Some(crate::catalog::AdapterSpec {
                reasoning_field: field.to_string(),
                thinking: "strip".to_string(),
                thinking_off: r#"{"enable_thinking":false}"#.to_string(),
                tool_choice: true,
                effort_field: "reasoning_effort".to_string(),
            }),
            ..catalog("vercel", endpoint)
        }
    }

    #[test]
    fn sse_line_reads_either_reasoning_field_into_the_host_only_chunk() {
        let p = OpenAiProvider::new();
        for field in ["reasoning_content", "reasoning"] {
            let line = format!(r#"data: {{"choices":[{{"delta":{{"{field}":"weigh it"}}}}]}}"#);
            let chunk = p.parse_sse_line(&line).unwrap().unwrap();
            assert_eq!(chunk.reasoning, "weigh it", "{field}");
            assert!(chunk.delta.is_empty() && chunk.tool_calls.is_empty());
        }
        // Both on one delta are counted once.
        let both = r#"data: {"choices":[{"delta":{"reasoning_content":"a","reasoning":"a"}}]}"#;
        assert_eq!(p.parse_sse_line(both).unwrap().unwrap().reasoning, "a");
        // A plain text delta carries no reasoning.
        let text = r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#;
        let chunk = p.parse_sse_line(text).unwrap().unwrap();
        assert_eq!((chunk.delta.as_str(), chunk.reasoning.as_str()), ("hi", ""));
    }

    #[tokio::test]
    async fn the_stream_loop_keeps_reasoning_apart_from_text_and_tool_calls() {
        for field in ["reasoning_content", "reasoning"] {
            let sse = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"{field}\":\"think \"}}}}]}}\n\n\
data: {{\"choices\":[{{\"delta\":{{\"{field}\":\"hard\"}}}}]}}\n\n\
data: {{\"choices\":[{{\"delta\":{{\"content\":\"ok\"}}}}]}}\n\n\
data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c1\",\"function\":{{\"name\":\"read_file\",\"arguments\":\"{{}}\"}}}}]}}}}]}}\n\n\
data: [DONE]\n\n"
            );
            let (port, _bodies) =
                serve_sequence(vec![http_response("200 OK", "text/event-stream", &sse)]).await;
            let provider = OpenAiProvider::new().with_retry_policy(fast_retry());
            let mut rx = provider
                .infer(
                    two_turn_request(),
                    reasoning_entry(&format!("http://127.0.0.1:{port}"), field),
                    Some(SecretString::new("key".to_string())),
                    &CachePlan::implicit(),
                )
                .await
                .unwrap();
            let (mut reasoning, mut text, mut calls) = (String::new(), String::new(), 0);
            while let Some(chunk) = rx.recv().await {
                let chunk = chunk.unwrap();
                reasoning.push_str(&chunk.reasoning);
                text.push_str(&chunk.delta);
                calls += chunk.tool_calls.len();
            }
            assert_eq!(
                (reasoning.as_str(), text.as_str(), calls),
                ("think hard", "ok", 1),
                "{field}"
            );
        }
    }

    fn wire_for(adapter: &vam_spec::harness::Adapter, entry: &ModelCatalogEntry) -> WireOptions {
        WireOptions {
            plan: crate::wire::plan_wire(entry, adapter, true),
            reattach: vec![],
        }
    }

    #[test]
    fn default_wire_options_leave_the_body_byte_identical() {
        for entry in [
            catalog("openai", "http://x"),
            reasoning_entry("http://x", "reasoning_content"),
        ] {
            let plain = build_openai_body(&two_turn_request(), &entry, &CachePlan::implicit());
            let wired = build_openai_body_with(
                &two_turn_request(),
                &entry,
                &CachePlan::implicit(),
                &wire_for(&vam_spec::harness::Adapter::default(), &entry),
            );
            assert_eq!(plain.to_string(), wired.to_string());
        }
    }

    #[test]
    fn tool_choice_and_effort_are_sent_only_when_asked_and_supported() {
        let adapter = vam_spec::harness::Adapter {
            tool_choice: "required".into(),
            effort: "high".into(),
            ..Default::default()
        };
        let entry = reasoning_entry("http://x", "reasoning_content");
        let body = build_openai_body_with(
            &two_turn_request(),
            &entry,
            &CachePlan::implicit(),
            &wire_for(&adapter, &entry),
        );
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["reasoning_effort"], "high");
        // Unsupported: neither is sent.
        let plain = catalog("openai", "http://x");
        let body = build_openai_body_with(
            &two_turn_request(),
            &plain,
            &CachePlan::implicit(),
            &wire_for(&adapter, &plain),
        );
        assert!(body.get("tool_choice").is_none() && body.get("reasoning_effort").is_none());
    }

    #[test]
    fn off_merges_the_entry_fragment_and_keep_reattaches_reasoning() {
        let entry = reasoning_entry("http://x", "reasoning_content");
        let mut adapter = vam_spec::harness::Adapter {
            thinking: "off".into(),
            ..Default::default()
        };
        let body = build_openai_body_with(
            &two_turn_request(),
            &entry,
            &CachePlan::implicit(),
            &wire_for(&adapter, &entry),
        );
        assert_eq!(body["enable_thinking"], false);

        adapter.thinking = "keep".into();
        let request = two_turn_request();
        let idx = request
            .messages
            .iter()
            .position(|m| m.role == "assistant")
            .unwrap();
        let mut wire = wire_for(&adapter, &entry);
        wire.reattach = vec![(idx, "earlier thoughts".to_string())];
        let body = build_openai_body_with(&request, &entry, &CachePlan::implicit(), &wire);
        assert_eq!(
            body["messages"][idx]["reasoning_content"],
            "earlier thoughts"
        );
        // Only that message carries it.
        assert_eq!(body.to_string().matches("earlier thoughts").count(), 1);
        // drop and strip never re-attach, even when handed text.
        for mode in ["drop", "strip"] {
            adapter.thinking = mode.into();
            let mut wire = wire_for(&adapter, &entry);
            wire.reattach = vec![(idx, "earlier thoughts".to_string())];
            let body = build_openai_body_with(&request, &entry, &CachePlan::implicit(), &wire);
            assert!(!body.to_string().contains("earlier thoughts"), "{mode}");
        }
    }
}
