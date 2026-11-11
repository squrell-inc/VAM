use bytes::Bytes;
use prost::Message;
use secrecy::SecretString;
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use vam_spec::v1::{ErrorStatus, InferenceRequest, InferenceResponseChunk, VamErrorCode};
use vam_vsock::{split_payload, Frame, FrameType, MessageReassembler, StreamChunk};

use crate::budget::SessionBudget;
use crate::cache::{is_downgraded, CacheKey, CacheKeySource, CachePlan, CacheTokens};
use crate::catalog::ModelCatalog;
use crate::error::VamInferenceError;
use crate::mock_script::MockScripts;
use crate::provider::{AnthropicProvider, InferenceProvider, MockProvider, OpenAiProvider};
use crate::wire::{plan_wire, ReasoningStore, ThinkingMode, WireOptions, MAX_KEPT_REASONING_BYTES};

pub trait SecretLookup: Send + Sync + std::fmt::Debug {
    fn get_secret(&self, secret_ref: &str) -> Option<SecretString>;
}

/// Host-side Cedar advertise filter for defense-in-depth tool list intersection.
pub trait AdvertisedToolsProvider: Send + Sync + std::fmt::Debug {
    fn advertised_tool_names(&self) -> Vec<String>;
}

#[derive(Debug)]
pub struct SimpleSecretLookup {
    secrets: Mutex<HashMap<String, SecretString>>,
}

impl Default for SimpleSecretLookup {
    fn default() -> Self {
        Self::new()
    }
}

impl SimpleSecretLookup {
    pub fn new() -> Self {
        Self {
            secrets: Mutex::new(HashMap::new()),
        }
    }

    pub fn insert(&self, key: &str, val: &str) {
        self.secrets
            .lock()
            .unwrap()
            .insert(key.to_string(), SecretString::new(val.to_string()));
    }
}

impl SecretLookup for SimpleSecretLookup {
    fn get_secret(&self, secret_ref: &str) -> Option<SecretString> {
        self.secrets.lock().unwrap().get(secret_ref).cloned()
    }
}

/// What the broker knows about the reasoning of one inference and the options the run
/// asked for, for `inference.completed`. All empty for a request that returned no
/// reasoning and asked for nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionExtras {
    /// Bytes of reasoning the model returned, under every thinking mode. `None` when none.
    pub reasoning_bytes: Option<i64>,
    /// Tool calls found as markup inside the reasoning (recorded, never run).
    pub reasoning_tool_markup: Option<i64>,
    /// Effective thinking mode; set when the model returned reasoning or the mode is not `drop`.
    pub reasoning_mode: Option<String>,
    /// Options the run asked for that were not sent or applied.
    pub adapter_notes: Vec<String>,
    /// 1 when this request is the one resample of an invalid call and the run's harness
    /// has `adapter.resampleOnInvalid`; `None` otherwise.
    pub resamples: Option<i64>,
}

/// Called on a session's first spend-budget refusal (`check_preflight`); the host records
/// `budget.hit` for the run bound to the session. Called again on later refusals: the host
/// keeps it idempotent.
pub type BudgetRefusalCallback = Arc<dyn Fn() + Send + Sync>;

pub type InferenceCompletionCallback = Arc<
    dyn Fn(&str, &str, i64, i64, i64, Option<i64>, &str, &str, CacheTokens, &CompletionExtras)
        + Send
        + Sync,
>;

/// The adapter switches of the run now bound to the session. Read for every request,
/// never cached: a session can serve several runs, each with its own stored harness.
pub type HarnessSource = Arc<dyn Fn() -> vam_spec::harness::Adapter + Send + Sync>;

/// Host-side inputs of the wire plan, grouped to keep `process_request` readable.
#[derive(Clone, Default)]
struct HostWire {
    harness: Option<HarnessSource>,
    reasoning: Arc<Mutex<ReasoningStore>>,
}

/// A request as it goes to the provider: after the host's tool intersect, with
/// the resolved catalog entry.
#[derive(Debug, Clone)]
pub struct ExchangeRequest {
    /// Unique per broker server; pairs a request with its response.
    pub exchange_id: u64,
    pub messages: Vec<vam_spec::v1::ChatMessage>,
    pub tools: Vec<vam_spec::v1::ToolSpec>,
    pub model: crate::catalog::ModelCatalogEntry,
    /// The guest's label for the request (a compaction's summary exchange).
    /// Untrusted: the observer verifies it against what it recorded.
    pub context_hint: Option<vam_spec::v1::ContextHint>,
}

/// What the provider answered, complete.
#[derive(Debug, Clone)]
pub struct ExchangeResponse {
    pub exchange_id: u64,
    pub text: String,
    pub tool_calls: Vec<vam_spec::v1::ToolCall>,
    pub usage: Option<vam_spec::v1::TokenUsage>,
    /// The reasoning text, only when the run's effective thinking mode is `strip`
    /// (cut to a fixed size). The host recorder keeps it off the main lane; it is never
    /// sent to the guest or back to the model.
    pub reasoning: Option<String>,
    /// True size of the reasoning in bytes (0 when none).
    pub reasoning_bytes: usize,
}

/// An observer's refusal of a request: the host could not record it, so the
/// broker does not forward it. `reason` is a stable token the guest can map
/// (`thread_log_quota_exceeded`, `thread_log_unavailable`); `message` is for
/// people and carries no host path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeRefusal {
    pub reason: String,
    pub message: String,
}

/// Sees every exchange the broker serves. An observer cannot fail a response:
/// `on_response` and `on_abort` return nothing. It can refuse a *request* the
/// host cannot record (a full thread log); the broker then answers the guest
/// with a typed error and never calls the provider or charges the budget.
pub trait ExchangeObserver: Send + Sync {
    /// Awaited before the request goes to the provider. `Err` refuses it.
    fn on_request(
        &self,
        request: ExchangeRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<(), ExchangeRefusal>>;
    /// Awaited before the final `end_of_message` frame is sent, so the response
    /// is durable before the guest can act on it.
    fn on_response(&self, response: ExchangeResponse) -> futures_util::future::BoxFuture<'_, ()>;
    /// The exchange ended without a complete response (provider error, stream
    /// error, budget error, the guest went away).
    fn on_abort(&self, exchange_id: u64);
}

/// Prefix of the message the guest sees when the provider says the prompt does
/// not fit the model's window. The guest compacts once and retries once.
pub const CONTEXT_TOO_LONG: &str = "context_too_long";

/// Most reasoning text the broker holds per reply: enough for `keep` to send back and for
/// the markup count. The size recorded is the true size.
const MAX_REASONING_BUFFER: usize = 1024 * 1024;

/// True when a provider's error body says the prompt was over the model's
/// context window (Anthropic "prompt is too long", OpenAI
/// `context_length_exceeded` / "maximum context length").
pub fn is_context_too_long(details: &str) -> bool {
    let d = details.to_ascii_lowercase();
    d.contains("prompt is too long")
        || d.contains("context_length_exceeded")
        || d.contains("maximum context length")
        || d.contains("exceeds the context window")
        || d.contains("request_too_large")
}

/// The message sent to the guest for a provider failure: a context-window
/// failure gets the stable `context_too_long:` prefix.
fn provider_error_message(e: &VamInferenceError) -> String {
    match e {
        VamInferenceError::ProviderError { details, .. } if is_context_too_long(details) => {
            format!("{CONTEXT_TOO_LONG}: the prompt does not fit the model's context window")
        }
        other => other.to_string(),
    }
}

/// The models a run may move to when its own model keeps failing (AR-4.8). Built by the host
/// for one request; `None` from the source means the run has no fallback (switch off, model
/// pinned by the caller, or no allowlist).
#[derive(Debug, Clone, Default)]
pub struct FallbackPlan {
    /// The operator allowlist in its order. Only these are ever tried, and never the failing one.
    pub candidates: Vec<String>,
    /// A test node (process-guest) answers every candidate from the scripted mock: it can reach
    /// no real provider, and the candidate keeps its own model id.
    pub serve_as_mock: bool,
}

/// Where a run's fallback plan comes from, read per request. The guest can neither set nor read it.
pub type FallbackSource = Arc<dyn Fn() -> Option<FallbackPlan> + Send + Sync>;

/// The one switch a task made: the model that failed, the one that took over, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFallback {
    pub from: String,
    pub to: String,
    /// The provider of `to`, as `model.resolved` names it.
    pub provider: String,
    pub reason: String,
}

/// Told once, when a task switches models.
pub type FallbackCallback = Arc<dyn Fn(&ModelFallback) + Send + Sync>;

#[derive(Debug, Clone)]
struct Switched {
    from: String,
    to: String,
    as_mock: bool,
}

/// Fallback state of one task's broker: where the plan comes from, who is told, and the switch
/// made (sticky: later requests for `from` go straight to `to`, and there is no second switch).
#[derive(Clone)]
struct FallbackHook {
    source: FallbackSource,
    on_switch: Option<FallbackCallback>,
    switched: Arc<std::sync::Mutex<Option<Switched>>>,
}

impl FallbackHook {
    fn current(&self) -> Option<Switched> {
        self.switched.lock().ok().and_then(|g| g.clone())
    }

    /// Takes the one switch. False when the task already switched.
    fn claim(&self, from: &str, to: &str, as_mock: bool) -> bool {
        let Ok(mut guard) = self.switched.lock() else {
            return false;
        };
        if guard.is_some() {
            return false;
        }
        *guard = Some(Switched {
            from: from.to_string(),
            to: to.to_string(),
            as_mock,
        });
        true
    }
}

/// Why a failed provider call may move to another model: 429, 529, 5xx, or a transport error.
/// Read only for an error that arrived before the first streamed byte. A 4xx (a bad request, a
/// refused key, a full context) is the request's fault and another model would refuse it too.
pub fn fallback_reason(error: &VamInferenceError) -> Option<String> {
    match error {
        VamInferenceError::RateLimited { .. } => Some("http_429".to_string()),
        VamInferenceError::ProviderError { details, .. } => {
            let code: u16 = details
                .strip_prefix("HTTP Status ")?
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()?;
            (code == 429 || code == 529 || (500..600).contains(&code))
                .then(|| format!("http_{code}"))
        }
        // The providers report a failed connection or send as `Internal`.
        VamInferenceError::Internal(_) => Some("transport".to_string()),
        _ => None,
    }
}

/// The first allowlisted model, other than `failed`, that the node can serve and has a key for.
fn pick_fallback(
    catalog: &ModelCatalog,
    secrets: &dyn SecretLookup,
    only_model: Option<&str>,
    plan: &FallbackPlan,
    failed: &str,
) -> Option<(crate::catalog::ModelCatalogEntry, Option<SecretString>)> {
    for name in &plan.candidates {
        if name == failed {
            continue;
        }
        let Some(mut entry) = catalog.get(name).cloned() else {
            continue;
        };
        if plan.serve_as_mock {
            entry.provider = "mock".to_string();
        } else if only_model.is_some_and(|only| only != name.as_str()) {
            continue;
        }
        if entry.provider == "mock" {
            return Some((entry, Some(SecretString::new("mock-key".to_string()))));
        }
        if let Some(key) = secrets.get_secret(&entry.secret_ref) {
            return Some((entry, Some(key)));
        }
    }
    None
}

pub struct InferenceBrokerServer {
    catalog: Arc<ModelCatalog>,
    budget: Arc<SessionBudget>,
    secrets: Arc<dyn SecretLookup>,
    advertised_tools: Option<Arc<dyn AdvertisedToolsProvider>>,
    completion_callback: Option<InferenceCompletionCallback>,
    budget_refusal: Option<BudgetRefusalCallback>,
    mock_scripts: Option<Arc<MockScripts>>,
    mock_trace: Option<crate::mock_script::MockTraceCallback>,
    only_model: Option<String>,
    exchange_observer: Option<Arc<dyn ExchangeObserver>>,
    cache_key_source: Option<CacheKeySource>,
    host_wire: HostWire,
    fallback: Option<FallbackHook>,
    next_exchange_id: Arc<std::sync::atomic::AtomicU64>,
}

impl std::fmt::Debug for InferenceBrokerServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceBrokerServer")
            .field("catalog", &self.catalog)
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

impl InferenceBrokerServer {
    pub fn new(
        catalog: Arc<ModelCatalog>,
        budget: Arc<SessionBudget>,
        secrets: Arc<dyn SecretLookup>,
    ) -> Self {
        Self::with_advertised_tools(catalog, budget, secrets, None)
    }

    pub fn with_advertised_tools(
        catalog: Arc<ModelCatalog>,
        budget: Arc<SessionBudget>,
        secrets: Arc<dyn SecretLookup>,
        advertised_tools: Option<Arc<dyn AdvertisedToolsProvider>>,
    ) -> Self {
        Self {
            catalog,
            budget,
            secrets,
            advertised_tools,
            completion_callback: None,
            budget_refusal: None,
            mock_scripts: None,
            mock_trace: None,
            only_model: None,
            exchange_observer: None,
            cache_key_source: None,
            host_wire: HostWire::default(),
            fallback: None,
            next_exchange_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    pub fn with_completion_callback(mut self, cb: InferenceCompletionCallback) -> Self {
        self.completion_callback = Some(cb);
        self
    }

    /// Runs `cb` when a call is refused because the session is over its spend budget.
    pub fn with_budget_refusal_callback(mut self, cb: BudgetRefusalCallback) -> Self {
        self.budget_refusal = Some(cb);
        self
    }

    /// Answers `mock` model requests that carry a `mock-script:<id>` marker from these scripts.
    pub fn with_mock_scripts(mut self, scripts: Arc<MockScripts>) -> Self {
        self.mock_scripts = Some(scripts);
        self
    }

    /// Serves only this model: a request that resolves to any other model is
    /// refused with `PermissionDenied`, before any secret lookup or provider call.
    pub fn with_only_model(mut self, model_name: impl Into<String>) -> Self {
        self.only_model = Some(model_name.into());
        self
    }

    /// Observes every exchange (request before the provider call, response
    /// before the final frame). See [`ExchangeObserver`].
    pub fn with_exchange_observer(mut self, observer: Arc<dyn ExchangeObserver>) -> Self {
        self.exchange_observer = Some(observer);
        self
    }

    /// Where the host-derived cache key comes from. Set by `vamd` when it
    /// builds the per-session broker; the guest can neither set nor read it.
    pub fn with_cache_key_source(mut self, source: CacheKeySource) -> Self {
        self.cache_key_source = Some(source);
        self
    }

    /// Where the run's `adapter` switches come from (thinking mode, `tool_choice`,
    /// effort). Set by `vamd` per session; the guest can neither set nor read it.
    /// Without it every request uses the defaults, which change nothing.
    pub fn with_harness_source(mut self, source: HarnessSource) -> Self {
        self.host_wire.harness = Some(source);
        self
    }

    /// Lets a task whose model keeps failing move once to another model the host names
    /// (AR-4.8). `source` says, per request, whether and where; `on_switch` is told once.
    pub fn with_fallback(
        mut self,
        source: FallbackSource,
        on_switch: Option<FallbackCallback>,
    ) -> Self {
        self.fallback = Some(FallbackHook {
            source,
            on_switch,
            switched: Arc::new(std::sync::Mutex::new(None)),
        });
        self
    }

    /// Receives one trace per scripted mock request (rule matched or unmatched).
    pub fn with_mock_trace_callback(mut self, cb: crate::mock_script::MockTraceCallback) -> Self {
        self.mock_trace = Some(cb);
        self
    }

    pub async fn handle_connection<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::channel::<Frame>(128);

        // Spawn writer loop
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if let Err(e) = frame.write_to(&mut writer).await {
                    eprintln!("[inference-broker] Server writer error: {}", e);
                    break;
                }
            }
        });

        // Reader loop with reassembler (max 10 MiB message limit)
        let mut reassembler = MessageReassembler::new(10 * 1024 * 1024);

        loop {
            match Frame::read_from(&mut reader).await {
                Ok(frame) => {
                    if frame.frame_type != FrameType::StreamInference {
                        continue;
                    }

                    match reassembler.handle_frame(&frame) {
                        Ok(Some(full_payload)) => {
                            let tx_clone = tx.clone();
                            let catalog = self.catalog.clone();
                            let budget = self.budget.clone();
                            let secrets = self.secrets.clone();
                            let advertised_tools = self.advertised_tools.clone();
                            let completion_callback = self.completion_callback.clone();
                            let budget_refusal = self.budget_refusal.clone();
                            let mock_scripts = self.mock_scripts.clone();
                            let mock_trace = self.mock_trace.clone();
                            let only_model = self.only_model.clone();
                            let exchange = self.exchange_observer.clone().map(|observer| {
                                let id = self
                                    .next_exchange_id
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                (observer, id)
                            });
                            let cache_key = self.cache_key_source.as_ref().and_then(|f| f());
                            let host_wire = self.host_wire.clone();
                            let fallback = self.fallback.clone();
                            let correlation_id = frame.correlation_id;

                            tokio::spawn(async move {
                                if let Err(e) = Self::process_request(
                                    correlation_id,
                                    full_payload,
                                    catalog,
                                    budget,
                                    secrets,
                                    advertised_tools,
                                    completion_callback,
                                    budget_refusal,
                                    mock_scripts,
                                    mock_trace,
                                    only_model,
                                    exchange,
                                    cache_key,
                                    host_wire,
                                    fallback,
                                    tx_clone,
                                )
                                .await
                                {
                                    eprintln!("[inference-broker] process_request error: {}", e);
                                }
                            });
                        }
                        Ok(None) => {} // Still reassembling
                        Err(e) => {
                            // Send error frame back
                            let err_chunk = InferenceResponseChunk {
                                delta: String::new(),
                                tool_calls: vec![],
                                usage: None,
                                error: Some(ErrorStatus {
                                    code: VamErrorCode::InvalidArgument as i32,
                                    message: format!("Reassembly error: {}", e),
                                    retryable: false,
                                    details: vec![],
                                }),
                                ..Default::default()
                            };
                            let mut payload = Vec::new();
                            err_chunk.encode(&mut payload).unwrap();
                            let response_frames = split_payload(
                                &payload,
                                frame.correlation_id,
                                FrameType::StreamInference,
                            );
                            for f in response_frames {
                                let _ = tx.send(f).await;
                            }
                        }
                    }
                }
                Err(e) => {
                    if e.kind() != io::ErrorKind::UnexpectedEof {
                        eprintln!("[inference-broker] Server reader error: {}", e);
                    }
                    break;
                }
            }
        }
    }

    // Each argument is a distinct per-request dependency; a params struct would be a refactor.
    #[allow(clippy::too_many_arguments)]
    async fn process_request(
        correlation_id: u32,
        payload: Bytes,
        catalog: Arc<ModelCatalog>,
        budget: Arc<SessionBudget>,
        secrets: Arc<dyn SecretLookup>,
        advertised_tools: Option<Arc<dyn AdvertisedToolsProvider>>,
        completion_callback: Option<InferenceCompletionCallback>,
        budget_refusal: Option<BudgetRefusalCallback>,
        mock_scripts: Option<Arc<MockScripts>>,
        mock_trace: Option<crate::mock_script::MockTraceCallback>,
        only_model: Option<String>,
        exchange: Option<(Arc<dyn ExchangeObserver>, u64)>,
        cache_key: Option<CacheKey>,
        host_wire: HostWire,
        fallback: Option<FallbackHook>,
        tx: mpsc::Sender<Frame>,
    ) -> Result<(), VamInferenceError> {
        let start_time = std::time::Instant::now();
        let mut total_prompt_tokens: i64 = 0;
        let mut total_completion_tokens: i64 = 0;
        let mut total_cache = CacheTokens::default();
        let mut total_spend_micro: Option<i64> = None;

        // 1. Decode InferenceRequest
        let mut req = InferenceRequest::decode(payload.as_ref())
            .map_err(|e| VamInferenceError::Serialization(e.to_string()))?;

        // 1b. Defense-in-depth: intersect guest tools with host-advertised names
        if let Some(ref provider) = advertised_tools {
            let allowed: std::collections::HashSet<String> =
                provider.advertised_tool_names().into_iter().collect();
            req.tools.retain(|t| allowed.contains(&t.name));
        }

        // 2. Pre-flight budget check
        if let Err(e) = budget.check_preflight() {
            if let Some(cb) = budget_refusal.as_ref() {
                cb();
            }
            Self::send_error(
                correlation_id,
                VamErrorCode::ResourceExhausted,
                &e.to_string(),
                &tx,
            )
            .await;
            return Ok(());
        }

        // 3. Model catalog resolution
        let entry = match catalog.resolve(req.model_preference.as_ref(), None) {
            Ok(ent) => ent,
            Err(e) => {
                Self::send_error(
                    correlation_id,
                    VamErrorCode::PermissionDenied,
                    &e.to_string(),
                    &tx,
                )
                .await;
                return Ok(());
            }
        };

        // 3b. A node pinned to one model (process-guest test nodes) never serves another.
        if let Some(only) = only_model.as_deref() {
            if entry.model_name != only {
                Self::send_error(
                    correlation_id,
                    VamErrorCode::PermissionDenied,
                    &format!(
                        "model '{}' is not served on this node (only '{}')",
                        entry.model_name, only
                    ),
                    &tx,
                )
                .await;
                return Ok(());
            }
        }

        // 3c. A task that already switched models stays on the model that took over.
        let mut entry = entry;
        if let Some(switched) = fallback.as_ref().and_then(FallbackHook::current) {
            if switched.from == entry.model_name {
                if let Some(next) = catalog.get(&switched.to) {
                    let mut next = next.clone();
                    if switched.as_mock {
                        next.provider = "mock".to_string();
                    }
                    entry = next;
                }
            }
        }

        // 4. Secret API Key lookup
        let api_key = if entry.provider == "mock" {
            Some(SecretString::new("mock-key".to_string()))
        } else {
            match secrets.get_secret(&entry.secret_ref) {
                Some(key) => Some(key),
                None => {
                    Self::send_error(
                        correlation_id,
                        VamErrorCode::Unauthenticated,
                        &format!(
                            "API key not found for provider '{}': bind the secret '{}' (needed for model '{}')",
                            entry.provider, entry.secret_ref, entry.model_name
                        ),
                        &tx,
                    )
                    .await;
                    return Ok(());
                }
            }
        };

        // 5. Select provider adapter
        if let Some(last_msg) = req.messages.last() {
            let role_prefix = match last_msg.role.as_str() {
                "user" => "👤 [User Instruction]",
                "tool" => "⚙️ [Tool Output]",
                "assistant" => "🤖 [Assistant Thought]",
                "system" => "📋 [System Prompt]",
                _ => "💬 [Message]",
            };
            let content_cleaned = last_msg.content.replace('\n', " ");
            let is_tool_error = last_msg.role == "tool"
                && (last_msg.content.contains("tool_error:")
                    || last_msg.content.contains("denied by security policy")
                    || last_msg.content.starts_with("error"));
            if is_tool_error {
                let preview: String = if content_cleaned.chars().count() > 500 {
                    format!(
                        "{}...",
                        content_cleaned.chars().take(500).collect::<String>()
                    )
                } else {
                    content_cleaned
                };
                println!("[broker:trace] ⚠️ [Tool Error Result] {}", preview);
            } else {
                let preview: String = if content_cleaned.chars().count() > 180 {
                    format!(
                        "{}...",
                        content_cleaned.chars().take(180).collect::<String>()
                    )
                } else {
                    content_cleaned
                };
                println!("[broker:trace] {} {}", role_prefix, preview);
            }
        }
        if let Some(sys_msg) = req.messages.iter().find(|m| m.role == "system") {
            if std::env::var("VAM_TRACE_PROMPT").ok().as_deref() == Some("1") {
                println!(
                    "[broker:trace] 📋 [System Prompt — FULL]\n{}",
                    sys_msg.content
                );
            } else {
                let content_cleaned = sys_msg.content.replace('\n', " ");
                let preview: String = if content_cleaned.chars().count() > 120 {
                    format!(
                        "{}...",
                        content_cleaned.chars().take(120).collect::<String>()
                    )
                } else {
                    content_cleaned
                };
                println!("[broker:trace] 📋 [System Prompt] {}", preview);
            }
        }
        let tool_names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
        println!(
            "[broker:trace] 🔧 [Tools Available] {} tool(s): {:?}",
            req.tools.len(),
            tool_names
        );

        let make_provider = |name: &str| -> Option<Box<dyn InferenceProvider>> {
            match name {
                "mock" => Some(Box::new(
                    match mock_scripts.clone() {
                        Some(scripts) => MockProvider::with_scripts(scripts),
                        None => MockProvider::new(),
                    }
                    .with_trace(mock_trace.clone()),
                )),
                "openai" | "openrouter" | "vercel" => Some(Box::new(OpenAiProvider::new())),
                "anthropic" => Some(Box::new(AnthropicProvider::new())),
                _ => None,
            }
        };
        let Some(provider) = make_provider(entry.provider.as_str()) else {
            Self::send_error(
                correlation_id,
                VamErrorCode::Unimplemented,
                &format!("Unsupported provider: {}", entry.provider),
                &tx,
            )
            .await;
            return Ok(());
        };

        // 5b. The observer sees the request as the provider will, before it is sent.
        let observer = exchange.as_ref().map(|(o, _)| o.clone());
        let exchange_id = exchange.as_ref().map(|(_, id)| *id).unwrap_or(0);
        if let Some(observer) = &observer {
            let seen = observer
                .on_request(ExchangeRequest {
                    exchange_id,
                    messages: req.messages.clone(),
                    tools: req.tools.clone(),
                    model: entry.clone(),
                    context_hint: req.context_hint.clone(),
                })
                .await;
            if let Err(refusal) = seen {
                // The conversation cannot be recorded: nothing goes to the
                // provider and nothing is charged.
                Self::send_error(
                    correlation_id,
                    VamErrorCode::ResourceExhausted,
                    &format!("{}: {}", refusal.reason, refusal.message),
                    &tx,
                )
                .await;
                return Ok(());
            }
        }
        let abort = |observer: &Option<Arc<dyn ExchangeObserver>>| {
            if let Some(observer) = observer {
                observer.on_abort(exchange_id);
            }
        };

        // 6. Execute inference and stream response chunks
        // The cache plan is built here from the catalog entry and the
        // host-derived key. Nothing in the request feeds it. A provider that
        // rejected the cache fields once gets none.
        let plan = if is_downgraded(&entry.provider, &entry.model_name) {
            CachePlan::implicit()
        } else {
            CachePlan::for_entry(&entry, cache_key.clone())
        };
        // The run's harness decides thinking, `tool_choice` and effort; the entry says what
        // the model supports. `keep` re-attaches remembered reasoning on the host.
        let adapter = host_wire.harness.as_ref().map(|f| f()).unwrap_or_default();
        let is_resample = vam_spec::compaction::is_resample_hint(req.context_hint.as_ref());
        let build_wire = |entry: &crate::catalog::ModelCatalogEntry, req: &InferenceRequest| {
            let mut wire = WireOptions {
                plan: plan_wire(entry, &adapter, !req.tools.is_empty()),
                reattach: Vec::new(),
            };
            if wire.plan.thinking == ThinkingMode::Keep {
                let (found, missing) = host_wire
                    .reasoning
                    .lock()
                    .map(|store| store.reattachments(&req.messages))
                    .unwrap_or_default();
                wire.reattach = found;
                if missing > 0 {
                    wire.plan.notes.push(format!("reasoning_missing:{missing}"));
                }
            }
            wire
        };
        let mut wire = build_wire(&entry, &req);
        // The fallback plan is read before the request is sent; a copy of the request is kept
        // only when there is one.
        let fallback_plan = fallback
            .as_ref()
            .filter(|hook| hook.current().is_none())
            .and_then(|hook| (hook.source)());
        let retry_req = fallback_plan.as_ref().map(|_| req.clone());
        let mut rx = match provider
            .infer_with(req, entry.clone(), api_key, &plan, &wire)
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                abort(&observer);
                Self::send_error(
                    correlation_id,
                    VamErrorCode::Internal,
                    &provider_error_message(&e),
                    &tx,
                )
                .await;
                return Ok(());
            }
        };
        // The first thing the provider sent. An error here came before any streamed byte: if it is
        // one a second model could cure, the task moves to the next allowlisted model, once.
        let mut pending = rx.recv().await;
        if let (Some(Err(error)), Some(fb_plan), Some(retry_req), Some(hook)) =
            (&pending, &fallback_plan, retry_req, &fallback)
        {
            if let Some(reason) = fallback_reason(error) {
                if let Some((next_entry, next_key)) = pick_fallback(
                    &catalog,
                    secrets.as_ref(),
                    only_model.as_deref(),
                    fb_plan,
                    &entry.model_name,
                ) {
                    if let Some(next_provider) = make_provider(next_entry.provider.as_str()) {
                        if hook.claim(
                            &entry.model_name,
                            &next_entry.model_name,
                            fb_plan.serve_as_mock,
                        ) {
                            let next_plan =
                                if is_downgraded(&next_entry.provider, &next_entry.model_name) {
                                    CachePlan::implicit()
                                } else {
                                    CachePlan::for_entry(&next_entry, cache_key.clone())
                                };
                            let next_wire = build_wire(&next_entry, &retry_req);
                            match next_provider
                                .infer_with(
                                    retry_req,
                                    next_entry.clone(),
                                    next_key,
                                    &next_plan,
                                    &next_wire,
                                )
                                .await
                            {
                                Ok(next_rx) => {
                                    if let Some(notify) = &hook.on_switch {
                                        notify(&ModelFallback {
                                            from: entry.model_name.clone(),
                                            to: next_entry.model_name.clone(),
                                            provider: next_entry.provider.clone(),
                                            reason,
                                        });
                                    }
                                    rx = next_rx;
                                    pending = rx.recv().await;
                                    entry = next_entry;
                                    wire = next_wire;
                                }
                                Err(e) => pending = Some(Err(e)),
                            }
                        }
                    }
                }
            }
        }
        let mut last_usage: Option<vam_spec::v1::TokenUsage> = None;
        let mut saw_error = false;

        let mut seq = 0u32;
        let mut accumulated_assistant = String::new();
        let mut accumulated_tool_calls: Vec<vam_spec::v1::ToolCall> = Vec::new();
        // Reasoning stays on the host: counted, kept (cut) for `strip` and `keep`, and
        // cleared from every chunk before it is encoded for the guest.
        let mut reasoning_bytes: usize = 0;
        let mut reasoning_text = String::new();
        loop {
            let next = if pending.is_some() {
                pending.take()
            } else {
                rx.recv().await
            };
            let Some(chunk_res) = next else { break };
            match chunk_res {
                Ok(mut chunk) => {
                    if !chunk.reasoning.is_empty() {
                        reasoning_bytes += chunk.reasoning.len();
                        if reasoning_text.len() < MAX_REASONING_BUFFER {
                            reasoning_text.push_str(crate::wire::truncate_utf8(
                                &chunk.reasoning,
                                MAX_REASONING_BUFFER - reasoning_text.len(),
                            ));
                        }
                        chunk.reasoning.clear();
                        // A chunk that carried only reasoning is not a chunk for the guest.
                        if chunk.delta.is_empty()
                            && chunk.tool_calls.is_empty()
                            && chunk.usage.is_none()
                            && chunk.error.is_none()
                        {
                            continue;
                        }
                    }
                    if !chunk.delta.is_empty() {
                        accumulated_assistant.push_str(&chunk.delta);
                    }
                    accumulated_tool_calls.extend(chunk.tool_calls.clone());
                    for tc in &chunk.tool_calls {
                        println!(
                            "[broker:trace] 🤖 [LLM Decision] Tool Call -> {}({})",
                            tc.name, tc.arguments
                        );
                    }

                    // Update budget post-flight if usage is reported
                    if chunk.error.is_some() {
                        saw_error = true;
                    }
                    if let Some(ref usage) = chunk.usage {
                        last_usage = Some(usage.clone());
                        println!(
                            "[broker:trace] 📊 [Token Usage] prompt: {}, completion: {}, total: {}",
                            usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                        );
                        total_prompt_tokens = usage.prompt_tokens as i64;
                        total_completion_tokens = usage.completion_tokens as i64;
                        total_cache = CacheTokens {
                            read: usage.cached_tokens as i64,
                            written: usage.cache_creation_tokens as i64,
                            unknown: usage.cached_tokens_unknown,
                        };
                        total_spend_micro = Some(entry.cost_micro(usage));
                        if let Err(e) = budget.record_exchange(usage, &entry) {
                            abort(&observer);
                            Self::send_error(
                                correlation_id,
                                VamErrorCode::ResourceExhausted,
                                &e.to_string(),
                                &tx,
                            )
                            .await;
                            return Ok(());
                        }
                    }

                    let mut chunk_payload = Vec::new();
                    chunk.encode(&mut chunk_payload).unwrap();

                    // Send as single-frame StreamChunk
                    let stream_chunk = StreamChunk {
                        seq,
                        end_of_message: false, // will send final end_of_message frame to close
                        payload: Bytes::from(chunk_payload),
                    };

                    let frame = Frame {
                        frame_type: FrameType::StreamInference,
                        correlation_id,
                        payload: stream_chunk.encode(),
                    };

                    if tx.send(frame).await.is_err() {
                        abort(&observer);
                        return Ok(());
                    }
                    seq += 1;
                }
                Err(e) => {
                    abort(&observer);
                    Self::send_error(
                        correlation_id,
                        VamErrorCode::Internal,
                        &provider_error_message(&e),
                        &tx,
                    )
                    .await;
                    return Ok(());
                }
            }
        }

        // The observer has the complete response before the guest can see the
        // stream end (decide, persist, act). A stream that carried an error is
        // not a response.
        if let Some(observer) = &observer {
            if saw_error {
                observer.on_abort(exchange_id);
            } else {
                let kept =
                    (wire.plan.thinking == ThinkingMode::Strip && reasoning_bytes > 0).then(|| {
                        crate::wire::truncate_utf8(&reasoning_text, MAX_KEPT_REASONING_BYTES)
                            .to_string()
                    });
                observer
                    .on_response(ExchangeResponse {
                        exchange_id,
                        text: accumulated_assistant.clone(),
                        tool_calls: accumulated_tool_calls.clone(),
                        usage: last_usage.clone(),
                        reasoning: kept,
                        reasoning_bytes,
                    })
                    .await;
            }
        }

        // Send final empty chunk to complete stream reassembly
        let final_stream_chunk = StreamChunk {
            seq,
            end_of_message: true,
            payload: Bytes::new(),
        };

        let final_frame = Frame {
            frame_type: FrameType::StreamInference,
            correlation_id,
            payload: final_stream_chunk.encode(),
        };

        let _ = tx.send(final_frame).await;

        // `keep`: remember this reply's reasoning for the next request's assistant message.
        if wire.plan.thinking == ThinkingMode::Keep && !saw_error && !reasoning_text.is_empty() {
            let calls: Vec<(String, String, String)> = accumulated_tool_calls
                .iter()
                .map(|c| (c.id.clone(), c.name.clone(), c.arguments.clone()))
                .collect();
            if let Ok(mut store) = host_wire.reasoning.lock() {
                store.remember(&accumulated_assistant, &calls, &reasoning_text);
            }
        }
        let extras = CompletionExtras {
            // Set only when the model returned reasoning; events of non-thinking
            // runs stay as they were.
            reasoning_bytes: (reasoning_bytes > 0).then_some(reasoning_bytes as i64),
            reasoning_tool_markup: (reasoning_bytes > 0)
                .then(|| {
                    vam_spec::text_tool_calls::extract_tool_calls_from_text(&reasoning_text).len()
                        as i64
                })
                .filter(|n| *n > 0),
            reasoning_mode: (reasoning_bytes > 0 || wire.plan.thinking != ThinkingMode::Drop)
                .then(|| wire.plan.thinking.as_str().to_string()),
            adapter_notes: wire.plan.notes.clone(),
            resamples: (is_resample && adapter.resample_on_invalid > 0).then_some(1),
        };

        if let Some(ref cb) = completion_callback {
            let duration_ms = start_time.elapsed().as_millis() as i64;
            let tool_calls_json = if accumulated_tool_calls.is_empty() {
                String::new()
            } else {
                let items: Vec<serde_json::Value> = accumulated_tool_calls
                    .iter()
                    .map(|tc| {
                        serde_json::json!({
                            "id": tc.id,
                            "name": tc.name,
                            "arguments": tc.arguments,
                        })
                    })
                    .collect();
                serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
            };
            cb(
                &entry.model_name,
                &entry.provider,
                total_prompt_tokens,
                total_completion_tokens,
                duration_ms,
                total_spend_micro,
                &accumulated_assistant,
                &tool_calls_json,
                total_cache,
                &extras,
            );
        }

        Ok(())
    }

    async fn send_error(
        correlation_id: u32,
        code: VamErrorCode,
        message: &str,
        tx: &mpsc::Sender<Frame>,
    ) {
        let err_chunk = InferenceResponseChunk {
            delta: String::new(),
            tool_calls: vec![],
            usage: None,
            error: Some(ErrorStatus {
                code: code as i32,
                message: message.to_string(),
                retryable: false,
                details: vec![],
            }),
            ..Default::default()
        };

        let mut payload = Vec::new();
        err_chunk.encode(&mut payload).unwrap();

        // Encode as final stream chunk
        let stream_chunk = StreamChunk {
            seq: 0,
            end_of_message: true,
            payload: Bytes::from(payload),
        };

        let frame = Frame {
            frame_type: FrameType::StreamInference,
            correlation_id,
            payload: stream_chunk.encode(),
        };

        let _ = tx.send(frame).await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::net::{UnixListener, UnixStream};
    use vam_spec::v1::{ChatMessage, ModelPreference};

    #[tokio::test]
    async fn test_broker_loopback_workflow() {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let catalog = Arc::new(ModelCatalog::new());
        let budget = Arc::new(SessionBudget::new(None));
        let secrets = Arc::new(SimpleSecretLookup::new());

        let server = Arc::new(InferenceBrokerServer::new(catalog, budget, secrets));

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                server.handle_connection(stream).await;
            }
        });

        let client_stream = UnixStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(client_stream);

        // 1. Prepare InferenceRequest
        let request = InferenceRequest {
            model_preference: Some(ModelPreference {
                provider: "mock".to_string(),
                model_name: "mock-model".to_string(),
                temperature: 0.7,
                top_p: 1.0,
                max_completion_tokens: 0,
            }),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "please run-echo now".to_string(),
                tool_calls: vec![],
                tool_call_id: String::new(),
            }],
            tools: vec![],
            context_hint: None,
        };

        let mut req_payload = Vec::new();
        request.encode(&mut req_payload).unwrap();

        // Send split payloads
        let frames = split_payload(&req_payload, 101, FrameType::StreamInference);
        for frame in frames {
            frame.write_to(&mut writer).await.unwrap();
        }

        // Receive response frames
        let mut got_tool_call = false;

        loop {
            let f = Frame::read_from(&mut reader).await.unwrap();
            assert_eq!(f.frame_type, FrameType::StreamInference);
            assert_eq!(f.correlation_id, 101);

            let chunk = StreamChunk::decode(&f.payload).unwrap();
            if chunk.end_of_message {
                break;
            }

            let resp_chunk = InferenceResponseChunk::decode(chunk.payload.as_ref()).unwrap();
            if !resp_chunk.tool_calls.is_empty() {
                assert_eq!(resp_chunk.tool_calls[0].name, "echo");
                got_tool_call = true;
            }
        }

        assert!(got_tool_call);
    }

    async fn first_chunk_error(
        only_model: Option<&str>,
        model: &str,
        provider: &str,
    ) -> Option<ErrorStatus> {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("b.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let mut server = InferenceBrokerServer::new(
            Arc::new(ModelCatalog::new()),
            Arc::new(SessionBudget::new(None)),
            Arc::new(SimpleSecretLookup::new()),
        );
        if let Some(m) = only_model {
            server = server.with_only_model(m);
        }
        let server = Arc::new(server);
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                server.handle_connection(stream).await;
            }
        });
        let client = UnixStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(client);
        let request = InferenceRequest {
            model_preference: Some(ModelPreference {
                provider: provider.to_string(),
                model_name: model.to_string(),
                temperature: 0.0,
                top_p: 1.0,
                max_completion_tokens: 0,
            }),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
                tool_calls: vec![],
                tool_call_id: String::new(),
            }],
            tools: vec![],
            context_hint: None,
        };
        let mut payload = Vec::new();
        request.encode(&mut payload).unwrap();
        for f in split_payload(&payload, 7, FrameType::StreamInference) {
            f.write_to(&mut writer).await.unwrap();
        }
        loop {
            let f = Frame::read_from(&mut reader).await.unwrap();
            let chunk = StreamChunk::decode(&f.payload).unwrap();
            if !chunk.payload.is_empty() {
                let resp = InferenceResponseChunk::decode(chunk.payload.as_ref()).unwrap();
                if resp.error.is_some() {
                    return resp.error;
                }
            }
            if chunk.end_of_message {
                return None;
            }
        }
    }

    #[tokio::test]
    async fn only_model_refuses_other_models_before_any_provider_call() {
        // The resolved model differs from the pinned one: refused.
        let err = first_chunk_error(Some("not-mock"), "mock-model", "mock")
            .await
            .expect("refused");
        assert_eq!(err.code, VamErrorCode::PermissionDenied as i32);
        assert!(err.message.contains("only 'not-mock'"), "{}", err.message);
        // A real-provider model is refused too, with no secret lookup error.
        let err = first_chunk_error(Some("mock-model"), "gpt-4o", "openai")
            .await
            .expect("refused");
        assert_eq!(err.code, VamErrorCode::PermissionDenied as i32);
    }

    #[derive(Default)]
    struct Recording {
        events: Mutex<Vec<String>>,
        requests: Mutex<Vec<ExchangeRequest>>,
        responses: Mutex<Vec<ExchangeResponse>>,
        /// Refuse every request with a quota reason.
        refuse: bool,
        /// Error messages the guest side received.
        errors: Mutex<Vec<String>>,
    }

    impl ExchangeObserver for Recording {
        fn on_request(
            &self,
            request: ExchangeRequest,
        ) -> futures_util::future::BoxFuture<'_, Result<(), ExchangeRefusal>> {
            Box::pin(async move {
                self.events.lock().unwrap().push("request".into());
                self.requests.lock().unwrap().push(request);
                if self.refuse {
                    return Err(ExchangeRefusal {
                        reason: "thread_log_quota_exceeded".into(),
                        message: "the thread log is full".into(),
                    });
                }
                Ok(())
            })
        }

        fn on_response(
            &self,
            response: ExchangeResponse,
        ) -> futures_util::future::BoxFuture<'_, ()> {
            Box::pin(async move {
                // Slow on purpose: the final frame must wait for it.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                self.events.lock().unwrap().push("response".into());
                self.responses.lock().unwrap().push(response);
            })
        }

        fn on_abort(&self, _exchange_id: u64) {
            self.events.lock().unwrap().push("abort".into());
        }
    }

    #[derive(Debug)]
    struct OnlyEcho;

    impl AdvertisedToolsProvider for OnlyEcho {
        fn advertised_tool_names(&self) -> Vec<String> {
            vec!["echo".to_string()]
        }
    }

    /// Sends one request and returns the events the observer had recorded when
    /// the final frame arrived.
    async fn exchange_with_observer(
        observer: Arc<Recording>,
        only_model: Option<&str>,
        tools: Vec<&str>,
    ) -> Vec<String> {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("o.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let mut server = InferenceBrokerServer::with_advertised_tools(
            Arc::new(ModelCatalog::new()),
            Arc::new(SessionBudget::new(None)),
            Arc::new(SimpleSecretLookup::new()),
            Some(Arc::new(OnlyEcho)),
        )
        .with_exchange_observer(observer.clone());
        if let Some(m) = only_model {
            server = server.with_only_model(m);
        }
        let server = Arc::new(server);
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                server.handle_connection(stream).await;
            }
        });
        let client = UnixStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(client);
        let request = InferenceRequest {
            model_preference: Some(ModelPreference {
                provider: "mock".to_string(),
                model_name: "mock-model".to_string(),
                temperature: 0.0,
                top_p: 1.0,
                max_completion_tokens: 0,
            }),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: "SYS".to_string(),
                    tool_calls: vec![],
                    tool_call_id: String::new(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: "please run-echo now".to_string(),
                    tool_calls: vec![],
                    tool_call_id: String::new(),
                },
            ],
            tools: tools
                .into_iter()
                .map(|name| vam_spec::v1::ToolSpec {
                    name: name.to_string(),
                    ..Default::default()
                })
                .collect(),
            context_hint: Some(vam_spec::v1::ContextHint {
                lane: vam_spec::v1::context_hint::Lane::Summary as i32,
                keep_from_index: 1,
                keep_from_msg_sha256: "abc".to_string(),
                reason: "tokens".to_string(),
            }),
        };
        let mut payload = Vec::new();
        request.encode(&mut payload).unwrap();
        for f in split_payload(&payload, 9, FrameType::StreamInference) {
            f.write_to(&mut writer).await.unwrap();
        }
        loop {
            let f = Frame::read_from(&mut reader).await.unwrap();
            let chunk = StreamChunk::decode(&f.payload).unwrap();
            if !chunk.payload.is_empty() {
                if let Ok(resp) = InferenceResponseChunk::decode(chunk.payload.as_ref()) {
                    if let Some(err) = resp.error {
                        observer.errors.lock().unwrap().push(err.message);
                    }
                }
            }
            if chunk.end_of_message {
                return observer.events.lock().unwrap().clone();
            }
        }
    }

    #[tokio::test]
    async fn observer_sees_the_filtered_request_and_the_response_before_the_final_frame() {
        let observer = Arc::new(Recording::default());
        let events =
            exchange_with_observer(observer.clone(), None, vec!["echo", "secret_tool"]).await;
        // The slow `on_response` had finished when the guest saw the stream end.
        assert_eq!(events, ["request", "response"]);
        let requests = observer.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        // The host's tool intersect applies before the observer sees the request.
        let names: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["echo"]);
        assert_eq!(requests[0].messages.len(), 2);
        assert_eq!(requests[0].model.model_name, "mock-model");
        // The guest's label reaches the observer untouched; the observer verifies it.
        let hint = requests[0]
            .context_hint
            .as_ref()
            .expect("the hint is passed on");
        assert_eq!(hint.keep_from_index, 1);
        assert_eq!(hint.reason, "tokens");
        let responses = observer.responses.lock().unwrap();
        assert_eq!(responses[0].exchange_id, requests[0].exchange_id);
        assert_eq!(responses[0].tool_calls[0].name, "echo");
    }

    #[tokio::test]
    async fn an_observer_can_refuse_a_request_it_cannot_record() {
        let observer = Arc::new(Recording {
            refuse: true,
            ..Recording::default()
        });
        let events = exchange_with_observer(observer.clone(), None, vec!["echo"]).await;
        // Seen, refused, never answered: no response was recorded and the
        // guest got the typed reason instead of a model reply.
        assert_eq!(events, ["request"]);
        assert!(observer.responses.lock().unwrap().is_empty());
        let errors = observer.errors.lock().unwrap();
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].starts_with("thread_log_quota_exceeded: "),
            "{}",
            errors[0]
        );
    }

    #[tokio::test]
    async fn a_refused_request_is_not_observed() {
        let observer = Arc::new(Recording::default());
        // The pinned model differs: the broker refuses before any provider call.
        let events = exchange_with_observer(observer.clone(), Some("not-mock"), vec![]).await;
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn provider_context_limit_errors_get_one_stable_code() {
        for body in [
            "HTTP Status 400: prompt is too long: 250000 tokens > 200000",
            "HTTP Status 400: {\"error\":{\"code\":\"context_length_exceeded\"}}",
            "This model's maximum context length is 128000 tokens",
        ] {
            let e = VamInferenceError::ProviderError {
                provider: "p".into(),
                details: body.into(),
            };
            assert!(
                provider_error_message(&e).starts_with("context_too_long: "),
                "{body}"
            );
        }
        let other = VamInferenceError::ProviderError {
            provider: "p".into(),
            details: "HTTP Status 500: overloaded".into(),
        };
        assert!(!provider_error_message(&other).starts_with("context_too_long"));
    }

    #[tokio::test]
    async fn only_model_serves_the_pinned_model() {
        assert!(first_chunk_error(Some("mock-model"), "mock-model", "mock")
            .await
            .is_none());
        assert!(first_chunk_error(None, "mock-model", "mock")
            .await
            .is_none());
    }

    /// A one-shot HTTP server: records the request body, answers `reply`.
    async fn fake_provider(
        reply: String,
    ) -> (u16, tokio::sync::oneshot::Receiver<serde_json::Value>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let body = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if buf.len() >= end + 4 + len {
                    break buf[end + 4..end + 4 + len].to_vec();
                }
            };
            let _ = tx.send(serde_json::from_slice(&body).unwrap());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        (port, rx)
    }

    #[tokio::test]
    async fn the_host_key_reaches_the_provider_and_cost_uses_cache_prices() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":10,\"prompt_tokens_details\":{\"cached_tokens\":800}}}\n\n\
data: [DONE]\n\n";
        let (port, body_rx) = fake_provider(sse.to_string()).await;

        let mut catalog = ModelCatalog::new();
        catalog.add_entry(crate::catalog::ModelCatalogEntry {
            provider: "openai".to_string(),
            model_name: "cache-key-test".to_string(),
            endpoint: format!("http://127.0.0.1:{port}"),
            secret_ref: "openai/api-key".to_string(),
            price_prompt_micro: 1_000_000,
            price_completion_micro: 2_000_000,
            price_cache_read_micro: 100_000,
            cache_mode: crate::cache::CacheMode::PromptKey,
            cache_retention: crate::cache::CacheRetention::Long,
            ..Default::default()
        });
        let entry = catalog.get("cache-key-test").unwrap().clone();
        let secrets = Arc::new(SimpleSecretLookup::new());
        secrets.insert("openai/api-key", "k");
        let budget = Arc::new(SessionBudget::new(None));
        let host_key = CacheKey::derive("owner", "agent-digest", "thread");
        let key_for_source = host_key.clone();
        let server = Arc::new(
            InferenceBrokerServer::new(Arc::new(catalog), budget.clone(), secrets)
                .with_cache_key_source(Arc::new(move || Some(key_for_source.clone()))),
        );

        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("b.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                server.handle_connection(stream).await;
            }
        });
        let client = UnixStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(client);
        // The guest tries to steer the cache: its text and hint carry cache words.
        let request = InferenceRequest {
            model_preference: Some(ModelPreference {
                provider: "openai".to_string(),
                model_name: "cache-key-test".to_string(),
                temperature: 0.0,
                top_p: 1.0,
                max_completion_tokens: 0,
            }),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "prompt_cache_key=attacker cache_control".to_string(),
                tool_calls: vec![],
                tool_call_id: String::new(),
            }],
            tools: vec![],
            context_hint: Some(vam_spec::v1::ContextHint {
                lane: vam_spec::v1::context_hint::Lane::Main as i32,
                keep_from_index: 0,
                keep_from_msg_sha256: "prompt_cache_key".to_string(),
                reason: "tokens".to_string(),
            }),
        };
        let mut payload = Vec::new();
        request.encode(&mut payload).unwrap();
        for f in split_payload(&payload, 9, FrameType::StreamInference) {
            f.write_to(&mut writer).await.unwrap();
        }
        let mut usage = None;
        loop {
            let f = Frame::read_from(&mut reader).await.unwrap();
            let chunk = StreamChunk::decode(&f.payload).unwrap();
            if !chunk.payload.is_empty() {
                let resp = InferenceResponseChunk::decode(chunk.payload.as_ref()).unwrap();
                assert!(resp.error.is_none(), "{:?}", resp.error);
                if resp.usage.is_some() {
                    usage = resp.usage;
                }
            }
            if chunk.end_of_message {
                break;
            }
        }

        let body = body_rx.await.unwrap();
        assert_eq!(body["prompt_cache_key"], host_key.as_str());
        assert_eq!(body["prompt_cache_retention"], "24h");
        assert!(!host_key.as_str().contains("owner"));

        let usage = usage.expect("usage reached the guest");
        assert_eq!(usage.cached_tokens, 800);
        assert!(!usage.cached_tokens_unknown);
        // 200 uncached x $1 + 800 read x $0.10 + 10 out x $2, in micro-USD.
        assert_eq!(entry.cost_micro(&usage), 200 + 80 + 20);
        assert_eq!(budget.get_accumulated_cost(), 300);
    }

    // ---- reasoning stays on the host (H-1.4) and harness options (H-1.5) ----

    /// Serves one SSE reply per connection, in order; sends each request body.
    async fn serve_replies(
        replies: Vec<String>,
    ) -> (u16, tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            for reply in replies {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let body = loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        break buf[end + 4..end + 4 + len].to_vec();
                    }
                };
                let _ = tx.send(serde_json::from_slice(&body).unwrap());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.len(),
                    reply
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (port, rx)
    }

    #[derive(Default)]
    struct Capture {
        responses: Mutex<Vec<ExchangeResponse>>,
    }

    impl ExchangeObserver for Capture {
        fn on_request(
            &self,
            _request: ExchangeRequest,
        ) -> futures_util::future::BoxFuture<'_, Result<(), ExchangeRefusal>> {
            Box::pin(async { Ok(()) })
        }
        fn on_response(
            &self,
            response: ExchangeResponse,
        ) -> futures_util::future::BoxFuture<'_, ()> {
            self.responses.lock().unwrap().push(response);
            Box::pin(async {})
        }
        fn on_abort(&self, _exchange_id: u64) {}
    }

    struct Rig {
        reader: tokio::io::ReadHalf<UnixStream>,
        writer: tokio::io::WriteHalf<UnixStream>,
        adapter: Arc<Mutex<vam_spec::harness::Adapter>>,
        extras: Arc<Mutex<Vec<CompletionExtras>>>,
        observer: Arc<Capture>,
        next_id: u32,
        model: (&'static str, &'static str),
        hint: Option<vam_spec::v1::ContextHint>,
        _dir: tempfile::TempDir,
    }

    fn think_entry(port: u16, tool_choice: bool) -> crate::catalog::ModelCatalogEntry {
        crate::catalog::ModelCatalogEntry {
            provider: "openai".to_string(),
            model_name: "think-test".to_string(),
            endpoint: format!("http://127.0.0.1:{port}"),
            secret_ref: "openai/api-key".to_string(),
            context_window: 100_000,
            supports_tools: true,
            adapter: Some(crate::catalog::AdapterSpec {
                reasoning_field: "reasoning_content".to_string(),
                thinking: "strip".to_string(),
                thinking_off: r#"{"enable_thinking":false}"#.to_string(),
                tool_choice,
                effort_field: String::new(),
            }),
            ..Default::default()
        }
    }

    async fn rig(entry: crate::catalog::ModelCatalogEntry, thinking: &str) -> Rig {
        let mut catalog = ModelCatalog::new();
        catalog.add_entry(entry);
        let secrets = Arc::new(SimpleSecretLookup::new());
        secrets.insert("openai/api-key", "k");
        let adapter = Arc::new(Mutex::new(vam_spec::harness::Adapter {
            thinking: thinking.to_string(),
            ..Default::default()
        }));
        let extras: Arc<Mutex<Vec<CompletionExtras>>> = Arc::default();
        let observer = Arc::new(Capture::default());
        let (a, e) = (adapter.clone(), extras.clone());
        let server = Arc::new(
            InferenceBrokerServer::new(
                Arc::new(catalog),
                Arc::new(SessionBudget::new(None)),
                secrets,
            )
            .with_harness_source(Arc::new(move || a.lock().unwrap().clone()))
            .with_completion_callback(Arc::new(move |_, _, _, _, _, _, _, _, _, x| {
                e.lock().unwrap().push(x.clone());
            }))
            .with_exchange_observer(observer.clone()),
        );
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("t.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                server.handle_connection(stream).await;
            }
        });
        let (reader, writer) = tokio::io::split(UnixStream::connect(&sock_path).await.unwrap());
        Rig {
            reader,
            writer,
            adapter,
            extras,
            observer,
            next_id: 1,
            model: ("openai", "think-test"),
            hint: None,
            _dir: dir,
        }
    }

    impl Rig {
        /// Sends one request; returns every payload frame the guest received.
        async fn ask(&mut self, messages: Vec<ChatMessage>, tools: &[&str]) -> Vec<Vec<u8>> {
            let request = InferenceRequest {
                model_preference: Some(ModelPreference {
                    provider: self.model.0.to_string(),
                    model_name: self.model.1.to_string(),
                    temperature: 0.0,
                    top_p: 1.0,
                    max_completion_tokens: 0,
                }),
                messages,
                tools: tools
                    .iter()
                    .map(|n| vam_spec::v1::ToolSpec {
                        name: n.to_string(),
                        ..Default::default()
                    })
                    .collect(),
                context_hint: self.hint.clone(),
            };
            let mut payload = Vec::new();
            request.encode(&mut payload).unwrap();
            self.next_id += 1;
            for f in split_payload(&payload, self.next_id, FrameType::StreamInference) {
                f.write_to(&mut self.writer).await.unwrap();
            }
            let mut frames = Vec::new();
            loop {
                let f = Frame::read_from(&mut self.reader).await.unwrap();
                let chunk = StreamChunk::decode(&f.payload).unwrap();
                if chunk.end_of_message {
                    return frames;
                }
                let resp = InferenceResponseChunk::decode(chunk.payload.as_ref()).unwrap();
                assert!(resp.error.is_none(), "{:?}", resp.error);
                frames.push(chunk.payload.to_vec());
            }
        }
    }

    fn user(text: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_string(),
            content: text.to_string(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        }
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
    }

    fn sse(reasoning: &str, text: &str, call: Option<&str>) -> String {
        let mut out = String::new();
        if !reasoning.is_empty() {
            out.push_str(&format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[{"delta":{"reasoning_content": reasoning}}]})
            ));
        }
        if !text.is_empty() {
            out.push_str(&format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[{"delta":{"content": text}}]})
            ));
        }
        if let Some(id) = call {
            out.push_str(&format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":id,"function":{"name":"read_file","arguments":"{}"}}]}}]})
            ));
        }
        out.push_str(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\ndata: [DONE]\n\n",
        );
        out
    }

    #[tokio::test]
    async fn reasoning_never_reaches_the_guest_and_is_recorded_under_every_mode() {
        let reasoning = "SECRET-R {\"name\":\"read_file\",\"arguments\":{}} end";
        for mode in ["drop", "strip", "off", "catalog"] {
            let (port, mut bodies) = serve_replies(vec![sse(reasoning, "ok", None)]).await;
            let mut rig = rig(think_entry(port, true), mode).await;
            let frames = rig.ask(vec![user("hi")], &[]).await;
            // Guest frames: the text and the usage, nothing else, and no reasoning bytes.
            assert!(frames.iter().all(|f| !contains(f, "SECRET-R")), "{mode}");
            let decoded: Vec<_> = frames
                .iter()
                .map(|f| InferenceResponseChunk::decode(f.as_slice()).unwrap())
                .collect();
            assert!(decoded.iter().all(|c| c.reasoning.is_empty()), "{mode}");
            assert_eq!(decoded.len(), 2, "{mode}");
            assert_eq!(decoded[0].delta, "ok");
            // The next request never carries it (nothing to re-attach under these modes).
            assert!(!bodies
                .recv()
                .await
                .unwrap()
                .to_string()
                .contains("SECRET-R"));
            let extras = rig.extras.lock().unwrap().clone();
            assert_eq!(extras.len(), 1);
            assert_eq!(
                extras[0].reasoning_bytes,
                Some(reasoning.len() as i64),
                "{mode}"
            );
            assert_eq!(extras[0].reasoning_tool_markup, Some(1), "{mode}");
            // `catalog` on this entry means strip.
            let want = if mode == "catalog" { "strip" } else { mode };
            assert_eq!(extras[0].reasoning_mode.as_deref(), Some(want));
            // Only strip hands the text to the recorder.
            let kept = rig.observer.responses.lock().unwrap()[0].reasoning.clone();
            assert_eq!(kept.is_some(), want == "strip", "{mode}");
            if let Some(kept) = kept {
                assert_eq!(kept, reasoning);
            }
        }
    }

    #[tokio::test]
    async fn a_reply_without_reasoning_and_default_options_adds_nothing() {
        let (port, mut bodies) = serve_replies(vec![sse("", "ok", None)]).await;
        let mut rig = rig(think_entry(port, true), "drop").await;
        rig.ask(vec![user("hi")], &[]).await;
        assert_eq!(rig.extras.lock().unwrap()[0], CompletionExtras::default());
        let body = bodies.recv().await.unwrap();
        for key in ["tool_choice", "enable_thinking", "reasoning_effort"] {
            assert!(body.get(key).is_none(), "{key}");
        }
    }

    #[tokio::test]
    async fn keep_reattaches_reasoning_on_the_host_when_the_next_request_is_built() {
        let (port, mut bodies) = serve_replies(vec![
            sse("R-ONE", "", Some("call_1")),
            sse("", "done", None),
            sse("", "again", None),
        ])
        .await;
        let mut rig = rig(think_entry(port, true), "keep").await;
        let first = rig.ask(vec![user("go")], &["read_file"]).await;
        assert!(first.iter().all(|f| !contains(f, "R-ONE")));
        assert!(!bodies.recv().await.unwrap().to_string().contains("R-ONE"));

        // The guest builds the next request from its own copy: no reasoning in it.
        let assistant = ChatMessage {
            role: "assistant".to_string(),
            content: String::new(),
            tool_calls: vec![vam_spec::v1::ToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: "{}".to_string(),
            }],
            tool_call_id: String::new(),
        };
        let tool = ChatMessage {
            role: "tool".to_string(),
            content: "contents".to_string(),
            tool_calls: vec![],
            tool_call_id: "call_1".to_string(),
        };
        let second = rig
            .ask(
                vec![user("go"), assistant.clone(), tool.clone()],
                &["read_file"],
            )
            .await;
        assert!(second.iter().all(|f| !contains(f, "R-ONE")));
        let body = bodies.recv().await.unwrap();
        assert_eq!(body["messages"][1]["reasoning_content"], "R-ONE");
        assert_eq!(
            rig.extras.lock().unwrap()[1].reasoning_mode.as_deref(),
            Some("keep")
        );

        // A tool call the broker never saw has nothing to re-attach: recorded.
        let mut stranger = assistant;
        stranger.tool_calls[0].id = "call_x".to_string();
        rig.ask(vec![user("go"), stranger, tool], &["read_file"])
            .await;
        let body = bodies.recv().await.unwrap();
        assert!(body["messages"][1].get("reasoning_content").is_none());
        assert_eq!(
            rig.extras.lock().unwrap()[2].adapter_notes,
            vec!["reasoning_missing:1".to_string()]
        );
    }

    #[tokio::test]
    async fn a_resample_is_counted_only_when_the_run_has_the_switch() {
        let (port, _bodies) = serve_replies(vec![
            sse("", "a", None),
            sse("", "b", None),
            sse("", "c", None),
            sse("", "d", None),
        ])
        .await;
        let mut rig = rig(think_entry(port, true), "drop").await;
        // No label: nothing recorded.
        rig.ask(vec![user("hi")], &[]).await;
        // Labelled, switch off: refused, nothing recorded.
        rig.hint = Some(vam_spec::v1::ContextHint {
            lane: vam_spec::v1::context_hint::Lane::Main as i32,
            reason: "resample".to_string(),
            ..Default::default()
        });
        rig.ask(vec![user("hi")], &[]).await;
        // Labelled, switch on: one resample, on either lane.
        rig.adapter.lock().unwrap().resample_on_invalid = 1;
        rig.ask(vec![user("hi")], &[]).await;
        let extras = rig.extras.lock().unwrap().clone();
        assert_eq!(
            extras.iter().map(|x| x.resamples).collect::<Vec<_>>(),
            vec![None, None, Some(1)]
        );
    }

    #[tokio::test]
    async fn tool_choice_follows_the_run_and_the_entry_and_unsupported_is_recorded() {
        for (supported, want_sent) in [(true, true), (false, false)] {
            let (port, mut bodies) = serve_replies(vec![sse("", "ok", None)]).await;
            let mut rig = rig(think_entry(port, supported), "drop").await;
            rig.adapter.lock().unwrap().tool_choice = "required".to_string();
            rig.ask(vec![user("hi")], &["read_file"]).await;
            let body = bodies.recv().await.unwrap();
            assert_eq!(body.get("tool_choice").is_some(), want_sent);
            let notes = rig.extras.lock().unwrap()[0].adapter_notes.clone();
            assert_eq!(notes.is_empty(), want_sent, "{notes:?}");
        }
    }

    #[tokio::test]
    async fn off_sends_the_entry_fragment() {
        let (port, mut bodies) = serve_replies(vec![sse("", "ok", None)]).await;
        let mut rig = rig(think_entry(port, true), "off").await;
        rig.ask(vec![user("hi")], &[]).await;
        assert_eq!(bodies.recv().await.unwrap()["enable_thinking"], false);
    }

    #[tokio::test]
    async fn the_mock_model_streams_scripted_reasoning_through_the_same_step() {
        let scripts = MockScripts::from_scripts(vec![serde_json::from_str(
            r#"{"id":"t","rules":[{"reply":{"reasoning":"mock-secret-3","text":"hi"}}]}"#,
        )
        .unwrap()])
        .unwrap();
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("m.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let extras: Arc<Mutex<Vec<CompletionExtras>>> = Arc::default();
        let e = extras.clone();
        let server = Arc::new(
            InferenceBrokerServer::new(
                Arc::new(ModelCatalog::new()),
                Arc::new(SessionBudget::new(None)),
                Arc::new(SimpleSecretLookup::new()),
            )
            .with_mock_scripts(Arc::new(scripts))
            .with_completion_callback(Arc::new(move |_, _, _, _, _, _, _, _, _, x| {
                e.lock().unwrap().push(x.clone());
            })),
        );
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                server.handle_connection(stream).await;
            }
        });
        let (reader, writer) = tokio::io::split(UnixStream::connect(&sock_path).await.unwrap());
        let mut rig = Rig {
            reader,
            writer,
            adapter: Arc::default(),
            extras: extras.clone(),
            observer: Arc::default(),
            next_id: 1,
            model: ("mock", "mock-model"),
            hint: None,
            _dir: dir,
        };
        let frames = rig
            .ask(
                vec![ChatMessage {
                    role: "system".to_string(),
                    content: "mock-script:t".to_string(),
                    tool_calls: vec![],
                    tool_call_id: String::new(),
                }],
                &[],
            )
            .await;
        assert!(frames.iter().all(|f| !contains(f, "mock-secret-3")));
        assert_eq!(extras.lock().unwrap()[0].reasoning_bytes, Some(13));
    }

    // ---- AR-4.8: fallback within the allowlist ----

    const FIRST: &str = "mock-model";
    const SECOND: &str = "openai/gpt-4o";

    fn fallback_scripts(first_reply: &str) -> Arc<MockScripts> {
        let script: crate::mock_script::MockScript = serde_json::from_str(&format!(
            r#"{{"id": "fb", "rules": [
                {{"when": {{"model": "{FIRST}"}}, "reply": {first_reply}}},
                {{"when": {{"model": "{SECOND}"}}, "reply": {{"text": "from the second"}}}}
            ]}}"#
        ))
        .unwrap();
        Arc::new(MockScripts::from_scripts(vec![script]).unwrap())
    }

    /// What one request got: its text, its error, and the switches the task had made by then.
    struct Outcome {
        text: String,
        error: Option<ErrorStatus>,
        switches: Vec<ModelFallback>,
    }

    /// Runs `requests` requests for `FIRST` through one broker (one task).
    async fn fallback_run(
        first_reply: &str,
        plan: Option<FallbackPlan>,
        requests: usize,
    ) -> Vec<Outcome> {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("fb.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let switches: Arc<Mutex<Vec<ModelFallback>>> = Arc::default();
        let mut server = InferenceBrokerServer::new(
            Arc::new(ModelCatalog::new()),
            Arc::new(SessionBudget::new(None)),
            Arc::new(SimpleSecretLookup::new()),
        )
        .with_mock_scripts(fallback_scripts(first_reply));
        if plan.is_some() {
            let seen = switches.clone();
            server = server.with_fallback(
                Arc::new(move || plan.clone()),
                Some(Arc::new(move |s: &ModelFallback| {
                    seen.lock().unwrap().push(s.clone())
                })),
            );
        }
        let server = Arc::new(server);
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                server.handle_connection(stream).await;
            }
        });
        let client = UnixStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(client);
        let mut outcomes = Vec::new();
        for n in 0..requests {
            let request = InferenceRequest {
                model_preference: Some(ModelPreference {
                    provider: "mock".to_string(),
                    model_name: FIRST.to_string(),
                    temperature: 0.0,
                    top_p: 1.0,
                    max_completion_tokens: 0,
                }),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "mock-script:fb hi".to_string(),
                    tool_calls: vec![],
                    tool_call_id: String::new(),
                }],
                tools: vec![],
                context_hint: None,
            };
            let mut payload = Vec::new();
            request.encode(&mut payload).unwrap();
            for f in split_payload(&payload, 10 + n as u32, FrameType::StreamInference) {
                f.write_to(&mut writer).await.unwrap();
            }
            let mut text = String::new();
            let mut error = None;
            loop {
                let f = Frame::read_from(&mut reader).await.unwrap();
                let chunk = StreamChunk::decode(&f.payload).unwrap();
                if !chunk.payload.is_empty() {
                    let resp = InferenceResponseChunk::decode(chunk.payload.as_ref()).unwrap();
                    text.push_str(&resp.delta);
                    error = error.or(resp.error);
                }
                if chunk.end_of_message {
                    break;
                }
            }
            outcomes.push(Outcome {
                text,
                error,
                switches: switches.lock().unwrap().clone(),
            });
        }
        outcomes
    }

    fn allowlist_plan() -> FallbackPlan {
        FallbackPlan {
            candidates: vec![FIRST.to_string(), SECOND.to_string()],
            serve_as_mock: true,
        }
    }

    #[tokio::test]
    async fn a_503_on_every_attempt_moves_to_the_next_allowed_model_once_and_stays() {
        let out = fallback_run(r#"{"fail": {"status": 503}}"#, Some(allowlist_plan()), 3).await;
        assert!(out[0].error.is_none(), "{:?}", out[0].error);
        assert_eq!(out[0].text, "from the second");
        assert_eq!(
            out[0].switches,
            vec![ModelFallback {
                from: FIRST.into(),
                to: SECOND.into(),
                provider: "mock".into(),
                reason: "http_503".into(),
            }]
        );
        // Sticky: later requests go straight to the second model, and no second switch is made.
        for later in &out[1..] {
            assert_eq!(later.text, "from the second");
            assert_eq!(later.switches.len(), 1);
        }
    }

    #[tokio::test]
    async fn a_429_529_and_a_5xx_move_but_a_4xx_does_not() {
        for status in [429u16, 529, 500, 502] {
            let reply = format!(r#"{{"fail": {{"status": {status}}}}}"#);
            let out = fallback_run(&reply, Some(allowlist_plan()), 1).await;
            assert_eq!(out[0].text, "from the second", "status {status}");
            assert_eq!(out[0].switches[0].reason, format!("http_{status}"));
        }
        for status in [400u16, 401, 403, 404, 413, 422] {
            let reply = format!(r#"{{"fail": {{"status": {status}}}}}"#);
            let out = fallback_run(&reply, Some(allowlist_plan()), 1).await;
            assert!(out[0].error.is_some(), "status {status} must surface");
            assert!(
                out[0].switches.is_empty(),
                "status {status} must not switch"
            );
            assert!(out[0].text.is_empty());
        }
    }

    #[tokio::test]
    async fn fallback_is_off_without_a_plan() {
        // No hook at all (the default), or a source that says no (switch off, model pinned by the
        // caller): the failure surfaces and nothing switches.
        let out = fallback_run(r#"{"fail": {"status": 503}}"#, None, 1).await;
        assert!(out[0].error.is_some());
        assert!(out[0].switches.is_empty());
    }

    #[tokio::test]
    async fn a_model_off_the_allowlist_is_never_chosen() {
        // The allowlist holds only the failing model: nothing to move to.
        let only_first = FallbackPlan {
            candidates: vec![FIRST.to_string()],
            serve_as_mock: true,
        };
        let out = fallback_run(r#"{"fail": {"status": 503}}"#, Some(only_first), 1).await;
        assert!(out[0].error.is_some());
        assert!(out[0].switches.is_empty());
        // A candidate the catalog does not know is skipped, not invented.
        let unknown = FallbackPlan {
            candidates: vec!["not-in-the-catalog".to_string()],
            serve_as_mock: true,
        };
        let out = fallback_run(r#"{"fail": {"status": 503}}"#, Some(unknown), 1).await;
        assert!(out[0].error.is_some());
        assert!(out[0].switches.is_empty());
    }

    #[tokio::test]
    async fn a_candidate_without_a_credential_is_skipped() {
        // Not a test node: the second model needs a real provider key, and the lookup has none.
        let plan = FallbackPlan {
            candidates: vec![FIRST.to_string(), SECOND.to_string()],
            serve_as_mock: false,
        };
        let out = fallback_run(r#"{"fail": {"status": 503}}"#, Some(plan), 1).await;
        assert!(out[0].error.is_some());
        assert!(out[0].switches.is_empty());
    }

    #[test]
    fn only_a_retryable_failure_before_the_first_byte_reads_as_a_reason() {
        let provider = |details: &str| VamInferenceError::ProviderError {
            provider: "openai".into(),
            details: details.into(),
        };
        assert_eq!(
            fallback_reason(&provider("HTTP Status 503 Service Unavailable: x")).as_deref(),
            Some("http_503")
        );
        assert_eq!(
            fallback_reason(&provider(
                "HTTP Status 529 <unknown status code>: overloaded"
            ))
            .as_deref(),
            Some("http_529")
        );
        assert_eq!(
            fallback_reason(&provider("HTTP Status 429 Too Many Requests: slow")).as_deref(),
            Some("http_429")
        );
        assert_eq!(
            fallback_reason(&VamInferenceError::Internal("connection reset".into())).as_deref(),
            Some("transport")
        );
        assert_eq!(
            fallback_reason(&provider("HTTP Status 400 Bad Request: x")),
            None
        );
        assert_eq!(
            fallback_reason(&provider("HTTP Status 401 Unauthorized: x")),
            None
        );
        assert_eq!(fallback_reason(&provider("something else")), None);
        assert_eq!(
            fallback_reason(&VamInferenceError::InvalidRequest { reason: "x".into() }),
            None
        );
    }

    #[test]
    fn the_pick_follows_allowlist_order_and_skips_the_failed_and_the_unserved() {
        let catalog = ModelCatalog::new();
        let secrets = SimpleSecretLookup::new();
        let plan = |names: &[&str], serve_as_mock| FallbackPlan {
            candidates: names.iter().map(|s| s.to_string()).collect(),
            serve_as_mock,
        };
        // A node pinned to one model serves nothing else.
        assert!(pick_fallback(
            &catalog,
            &secrets,
            Some(FIRST),
            &plan(&[FIRST, SECOND], false),
            FIRST
        )
        .is_none());
        // Without a key the real model is skipped; with one it is picked.
        assert!(pick_fallback(
            &catalog,
            &secrets,
            None,
            &plan(&[FIRST, SECOND], false),
            FIRST
        )
        .is_none());
        let secret_ref = catalog.get(SECOND).unwrap().secret_ref.clone();
        secrets.insert(&secret_ref, "k");
        let picked = pick_fallback(
            &catalog,
            &secrets,
            None,
            &plan(&[FIRST, SECOND], false),
            FIRST,
        )
        .expect("a model with a key");
        assert_eq!(picked.0.model_name, SECOND);
        // The failed model is never its own fallback, and a test node answers from the mock.
        assert!(pick_fallback(&catalog, &secrets, None, &plan(&[SECOND], true), SECOND).is_none());
        let picked = pick_fallback(&catalog, &secrets, None, &plan(&[SECOND], true), FIRST)
            .expect("served as the mock");
        assert_eq!(
            (picked.0.model_name.as_str(), picked.0.provider.as_str()),
            (SECOND, "mock")
        );
    }
}
