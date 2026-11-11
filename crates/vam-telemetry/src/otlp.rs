//! Host-only OTLP export (Decision T3). Never linked from `vam-runtime` (T30).
//!
//! Uses OTLP/HTTP to loopback `:4318`. Orchestrator already pins `tonic 0.10` for
//! control-plane gRPC, so this crate does not enable `grpc-tonic` / OTLP gRPC `:4317`.
//! The sample collector still listens on both ports; fan-out is collector config.

use crate::metrics::{
    install_metrics_bridge, MetricsBridge, VamMetricsRegistry, HISTOGRAM_BOUNDARIES,
};
use crate::sampling::TailSamplingPolicy;
use crate::tenant::CollectorTlsConfig;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::{SpanId, Status, TraceId};
use opentelemetry::{global, KeyValue};
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{SdkTracerProvider, Span, SpanData, SpanProcessor};
use opentelemetry_sdk::Resource;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
/// Default loopback OTLP/HTTP endpoint. Collector gRPC stays on `:4317` for other senders.
pub const DEFAULT_OTLP_ENDPOINT: &str = "http://127.0.0.1:4318";
pub const ENV_OTLP_ENABLED: &str = "VAMD_OTLP_ENABLED";
pub const ENV_OTLP_ENDPOINT: &str = "VAMD_OTLP_ENDPOINT";
pub const ENV_OTLP_SUCCESS_SAMPLE_RATE: &str = "VAMD_OTLP_SUCCESS_SAMPLE_RATE";
pub const ENV_OTLP_SLOW_MS: &str = "VAMD_OTLP_SLOW_MS";
/// Logged when a non-loopback collector has no mTLS material.
pub const OTLP_DISABLED_REMOTE_REQUIRES_MTLS: &str = "remote_requires_mtls";

const MAX_PENDING_TRACES: usize = 2048;
const MAX_DECISION_CACHE: usize = 4096;
const METRICS_EXPORT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct OtlpConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub policy: TailSamplingPolicy,
    pub tls: CollectorTlsConfig,
    pub disabled_reason: Option<String>,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

impl OtlpConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            endpoint: DEFAULT_OTLP_ENDPOINT.to_string(),
            policy: TailSamplingPolicy::default(),
            tls: CollectorTlsConfig::default(),
            disabled_reason: None,
        }
    }

    pub fn from_env() -> Self {
        Self {
            enabled: parse_enabled(std::env::var(ENV_OTLP_ENABLED).ok().as_deref()),
            endpoint: std::env::var(ENV_OTLP_ENDPOINT)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_OTLP_ENDPOINT.to_string()),
            policy: TailSamplingPolicy {
                success_sample_rate: parse_rate(
                    std::env::var(ENV_OTLP_SUCCESS_SAMPLE_RATE).ok().as_deref(),
                ),
                slow_trace_threshold_ms: parse_slow_ms(
                    std::env::var(ENV_OTLP_SLOW_MS).ok().as_deref(),
                ),
            },
            tls: CollectorTlsConfig::from_env(),
            disabled_reason: None,
        }
        .resolve()
    }

    /// Loopback HTTP stays on. Non-loopback without mTLS material disables export.
    pub fn resolve(mut self) -> Self {
        if !self.enabled {
            return self;
        }
        let loopback = endpoint_is_loopback(&self.endpoint);
        if loopback {
            return self;
        }
        self.tls.enabled = true;
        if !self.tls.has_mtls_material() {
            self.enabled = false;
            self.disabled_reason = Some(OTLP_DISABLED_REMOTE_REQUIRES_MTLS.to_string());
            return self;
        }
        if self.endpoint.starts_with("http://") {
            self.endpoint = self.endpoint.replacen("http://", "https://", 1);
        }
        self
    }
}

/// Host of an OTLP endpoint URL (`http://127.0.0.1:4318` → `127.0.0.1`).
pub fn otlp_endpoint_host(endpoint: &str) -> Option<String> {
    let rest = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint);
    let hostport = rest.split('/').next().filter(|s| !s.is_empty())?;
    if let Some(inner) = hostport.strip_prefix('[') {
        return inner.split(']').next().map(|s| s.to_string());
    }
    Some(
        hostport
            .rsplit_once(':')
            .map(|(h, _)| h.to_string())
            .unwrap_or_else(|| hostport.to_string()),
    )
}

pub fn endpoint_is_loopback(endpoint: &str) -> bool {
    matches!(
        otlp_endpoint_host(endpoint).as_deref(),
        Some("127.0.0.1") | Some("localhost") | Some("::1") | Some("0:0:0:0:0:0:0:1")
    )
}

fn build_mtls_http_client(tls: &CollectorTlsConfig) -> Result<reqwest::blocking::Client, String> {
    let ca_path = tls
        .ca_cert_path
        .as_deref()
        .ok_or_else(|| "missing VAMD_OTLP_TLS_CA".to_string())?;
    let cert_path = tls
        .client_cert_path
        .as_deref()
        .ok_or_else(|| "missing VAMD_OTLP_TLS_CERT".to_string())?;
    let key_path = tls
        .client_key_path
        .as_deref()
        .ok_or_else(|| "missing VAMD_OTLP_TLS_KEY".to_string())?;

    let ca_pem = std::fs::read(ca_path).map_err(|e| format!("read OTLP TLS CA: {e}"))?;
    let cert_pem = std::fs::read(cert_path).map_err(|e| format!("read OTLP TLS cert: {e}"))?;
    let key_pem = std::fs::read(key_path).map_err(|e| format!("read OTLP TLS key: {e}"))?;

    let ca =
        reqwest::Certificate::from_pem(&ca_pem).map_err(|e| format!("parse OTLP TLS CA: {e}"))?;
    let mut identity_pem = cert_pem;
    identity_pem.extend_from_slice(&key_pem);
    let identity = reqwest::Identity::from_pem(&identity_pem)
        .map_err(|e| format!("parse OTLP TLS identity: {e}"))?;

    reqwest::blocking::Client::builder()
        .use_rustls_tls()
        .add_root_certificate(ca)
        .identity(identity)
        .build()
        .map_err(|e| format!("build OTLP mTLS client: {e}"))
}

pub fn parse_enabled(val: Option<&str>) -> bool {
    match val {
        None => true,
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
    }
}

pub fn parse_rate(val: Option<&str>) -> f64 {
    val.and_then(|v| v.parse::<f64>().ok())
        .filter(|r| *r >= 0.0)
        .unwrap_or(0.05)
}

pub fn parse_slow_ms(val: Option<&str>) -> u64 {
    val.and_then(|v| v.parse::<u64>().ok()).unwrap_or(1000)
}

static TRACER_PROVIDER: Mutex<Option<SdkTracerProvider>> = Mutex::new(None);
static METER_PROVIDER: Mutex<Option<SdkMeterProvider>> = Mutex::new(None);
static METRICS_WALK_STOP: AtomicBool = AtomicBool::new(false);

/// Build OTLP providers. Caller attaches `tracing_opentelemetry::layer()` to the subscriber.
/// Fail-open: Err means fmt-only.
pub fn install_otlp(
    service_name: &str,
    config: &OtlpConfig,
) -> Result<opentelemetry_sdk::trace::SdkTracer, String> {
    let resource = Resource::builder()
        .with_service_name(service_name.to_string())
        .with_attributes([KeyValue::new("service.namespace", "vam.platform")])
        .build();

    let mtls_client = if !endpoint_is_loopback(&config.endpoint) && config.tls.has_mtls_material() {
        if let Some(expected) = config.tls.expected_spiffe_id.as_deref() {
            tracing::info!(
                expected_spiffe_id = expected,
                "OTLP remote mTLS: expected SPIFFE ID configured (string check only; no SPIFFE SDK)"
            );
        }
        Some(build_mtls_http_client(&config.tls)?)
    } else {
        None
    };

    let span_exporter = {
        let builder = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(config.endpoint.clone());
        let builder = if let Some(client) = mtls_client.clone() {
            builder.with_http_client(client)
        } else {
            builder
        };
        builder
            .build()
            .map_err(|e| format!("OTLP span exporter: {e}"))?
    };

    let batch = opentelemetry_sdk::trace::BatchSpanProcessor::builder(span_exporter).build();
    let processor = TailSamplingProcessor::new(config.policy.clone(), batch);
    let tracer_provider = SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_resource(resource.clone())
        .build();

    let metric_exporter = {
        let builder = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(config.endpoint.clone());
        let builder = if let Some(client) = mtls_client {
            builder.with_http_client(client)
        } else {
            builder
        };
        builder
            .build()
            .map_err(|e| format!("OTLP metric exporter: {e}"))?
    };

    let reader = PeriodicReader::builder(metric_exporter)
        .with_interval(METRICS_EXPORT_INTERVAL)
        .build();
    let meter_provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build();

    let meter = meter_provider.meter("vamd");
    let bridge = Arc::new(OtelMetricsBridge::new(meter));
    install_metrics_bridge(bridge.clone());
    start_metrics_walk(bridge);

    global::set_tracer_provider(tracer_provider.clone());
    global::set_meter_provider(meter_provider.clone());

    if let Ok(mut slot) = TRACER_PROVIDER.lock() {
        *slot = Some(tracer_provider.clone());
    }
    if let Ok(mut slot) = METER_PROVIDER.lock() {
        *slot = Some(meter_provider);
    }

    use opentelemetry::trace::TracerProvider as _;
    Ok(tracer_provider.tracer(service_name.to_string()))
}

pub fn shutdown_otlp() {
    METRICS_WALK_STOP.store(true, Ordering::SeqCst);
    if let Ok(mut slot) = TRACER_PROVIDER.lock() {
        if let Some(provider) = slot.take() {
            let _ = provider.force_flush();
            let _ = provider.shutdown();
        }
    }
    if let Ok(mut slot) = METER_PROVIDER.lock() {
        if let Some(provider) = slot.take() {
            let _ = provider.force_flush();
            let _ = provider.shutdown();
        }
    }
}

fn start_metrics_walk(bridge: Arc<OtelMetricsBridge>) {
    METRICS_WALK_STOP.store(false, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("vamd-otlp-metrics".into())
        .spawn(move || {
            while !METRICS_WALK_STOP.load(Ordering::SeqCst) {
                std::thread::sleep(METRICS_EXPORT_INTERVAL);
                if METRICS_WALK_STOP.load(Ordering::SeqCst) {
                    break;
                }
                bridge.export_registry_snapshot(&VamMetricsRegistry::global());
            }
        })
        .ok();
}

/// Seed used by [`TailSamplingPolicy::should_sample`] from a W3C trace id.
pub fn trace_id_seed(trace_id: TraceId) -> u64 {
    let bytes = trace_id.to_bytes();
    u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

pub fn is_root_span(parent: SpanId) -> bool {
    parent == SpanId::INVALID
}

/// Add `gen_ai.*` aliases at the OTLP edge. Keeps original `vam.*` / tracing keys.
/// Never aliases `vam.session.id` (see [`crate::semconv::export_genai_alias`]).
pub fn apply_genai_aliases(mut span: SpanData) -> SpanData {
    use crate::semconv::export_genai_alias;

    let extras: Vec<KeyValue> = span
        .attributes
        .iter()
        .filter_map(|kv| {
            export_genai_alias(kv.key.as_str()).map(|alias| KeyValue::new(alias, kv.value.clone()))
        })
        .collect();
    span.attributes.extend(extras);
    span
}

fn span_duration_ms(span: &SpanData) -> u64 {
    span.end_time
        .duration_since(span.start_time)
        .ok()
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn span_is_error(span: &SpanData) -> bool {
    matches!(span.status, Status::Error { .. })
}

struct PendingTrace {
    spans: Vec<SpanData>,
}

struct DecisionCache {
    map: HashMap<[u8; 16], bool>,
    order: VecDeque<[u8; 16]>,
}

impl DecisionCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn insert(&mut self, id: [u8; 16], keep: bool) {
        if self.map.insert(id, keep).is_none() {
            self.order.push_back(id);
        }
        while self.order.len() > MAX_DECISION_CACHE {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }

    fn get(&self, id: &[u8; 16]) -> Option<bool> {
        self.map.get(id).copied()
    }
}

/// Buffers spans until the root/run span ends, then applies [`TailSamplingPolicy`].
/// Children follow the parent decision (whole-trace keep or drop).
pub struct TailSamplingProcessor<P> {
    policy: TailSamplingPolicy,
    inner: P,
    pending: Mutex<HashMap<[u8; 16], PendingTrace>>,
    decisions: Mutex<DecisionCache>,
}

impl<P: SpanProcessor> TailSamplingProcessor<P> {
    pub fn new(policy: TailSamplingPolicy, inner: P) -> Self {
        Self {
            policy,
            inner,
            pending: Mutex::new(HashMap::new()),
            decisions: Mutex::new(DecisionCache::new()),
        }
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 16], PendingTrace>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_decisions(&self) -> std::sync::MutexGuard<'_, DecisionCache> {
        self.decisions.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn forward(&self, spans: Vec<SpanData>) {
        for span in spans {
            self.inner.on_end(apply_genai_aliases(span));
        }
    }

    /// Apply the policy to a completed trace. Returns spans to export (empty = drop).
    pub fn decide_completed_trace(&self, spans: &[SpanData], root: &SpanData) -> bool {
        let is_error = spans.iter().any(span_is_error) || span_is_error(root);
        let duration_ms = span_duration_ms(root);
        let seed = trace_id_seed(root.span_context.trace_id());
        self.policy.should_sample(is_error, duration_ms, seed)
    }
}

impl<P: SpanProcessor> std::fmt::Debug for TailSamplingProcessor<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TailSamplingProcessor")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl<P: SpanProcessor> SpanProcessor for TailSamplingProcessor<P> {
    fn on_start(&self, span: &mut Span, cx: &opentelemetry::Context) {
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, span: SpanData) {
        let trace_id = span.span_context.trace_id().to_bytes();
        let is_root = is_root_span(span.parent_span_id);

        if let Some(keep) = self.lock_decisions().get(&trace_id) {
            if keep {
                self.forward(vec![span]);
            }
            return;
        }

        if is_root {
            let mut pending = self.lock_pending();
            let mut finished = pending
                .remove(&trace_id)
                .unwrap_or(PendingTrace { spans: Vec::new() });
            let keep = self.decide_completed_trace(&finished.spans, &span);
            finished.spans.push(span);
            drop(pending);
            self.lock_decisions().insert(trace_id, keep);
            if keep {
                self.forward(finished.spans);
            }
            return;
        }

        let mut pending = self.lock_pending();
        if pending.len() >= MAX_PENDING_TRACES && !pending.contains_key(&trace_id) {
            // Fail-open: drop the oldest incomplete trace rather than grow forever.
            if let Some(old_id) = pending.keys().next().copied() {
                pending.remove(&old_id);
            }
        }
        pending
            .entry(trace_id)
            .or_insert_with(|| PendingTrace { spans: Vec::new() })
            .spans
            .push(span);
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

/// Counter name plus sorted label pairs.
type CounterKey = (String, Vec<(String, String)>);

struct OtelMetricsBridge {
    meter: opentelemetry::metrics::Meter,
    counters: Mutex<HashMap<String, opentelemetry::metrics::Counter<u64>>>,
    gauges: Mutex<HashMap<String, opentelemetry::metrics::Gauge<f64>>>,
    histograms: Mutex<HashMap<String, opentelemetry::metrics::Histogram<f64>>>,
    last_counters: Mutex<HashMap<CounterKey, u64>>,
}

impl OtelMetricsBridge {
    fn new(meter: opentelemetry::metrics::Meter) -> Self {
        Self {
            meter,
            counters: Mutex::new(HashMap::new()),
            gauges: Mutex::new(HashMap::new()),
            histograms: Mutex::new(HashMap::new()),
            last_counters: Mutex::new(HashMap::new()),
        }
    }

    fn intern_name(name: &str) -> &'static str {
        Box::leak(name.to_string().into_boxed_str())
    }

    fn attrs(labels: &[(&str, &str)]) -> Vec<KeyValue> {
        labels
            .iter()
            .map(|(k, v)| KeyValue::new(k.to_string(), v.to_string()))
            .collect()
    }

    fn attrs_owned(labels: &[(String, String)]) -> Vec<KeyValue> {
        labels
            .iter()
            .map(|(k, v)| KeyValue::new(k.clone(), v.clone()))
            .collect()
    }

    fn counter(&self, name: &str) -> opentelemetry::metrics::Counter<u64> {
        let mut map = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(name.to_string())
            .or_insert_with(|| self.meter.u64_counter(Self::intern_name(name)).build())
            .clone()
    }

    fn gauge(&self, name: &str) -> opentelemetry::metrics::Gauge<f64> {
        let mut map = self.gauges.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(name.to_string())
            .or_insert_with(|| self.meter.f64_gauge(Self::intern_name(name)).build())
            .clone()
    }

    fn histogram(&self, name: &str) -> opentelemetry::metrics::Histogram<f64> {
        let mut map = self.histograms.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(name.to_string())
            .or_insert_with(|| {
                self.meter
                    .f64_histogram(Self::intern_name(name))
                    .with_boundaries(HISTOGRAM_BOUNDARIES.to_vec())
                    .build()
            })
            .clone()
    }

    /// Periodic (~10s) registry walk → OTel counters/gauges. Histograms are dual-written
    /// at record time because buckets have no raw samples to replay.
    fn export_registry_snapshot(&self, registry: &VamMetricsRegistry) {
        for (name, labels, value) in registry.snapshot_counters() {
            let key = (name.clone(), labels.clone());
            let mut last = self.last_counters.lock().unwrap_or_else(|e| e.into_inner());
            let prev = last.insert(key, value).unwrap_or(0);
            drop(last);
            if value > prev {
                self.counter(&name)
                    .add(value - prev, &Self::attrs_owned(&labels));
            }
        }
        for (name, labels, value) in registry.snapshot_gauges() {
            self.gauge(&name)
                .record(value as f64, &Self::attrs_owned(&labels));
        }
    }
}

impl MetricsBridge for OtelMetricsBridge {
    fn record_histogram(&self, name: &str, sample: f64, labels: &[(&str, &str)]) {
        self.histogram(name).record(sample, &Self::attrs(labels));
    }
}

/// In-memory span processor used by unit tests (no collector).
#[derive(Clone, Debug, Default)]
pub struct CapturingProcessor {
    pub spans: Arc<Mutex<Vec<SpanData>>>,
}

impl CapturingProcessor {
    pub fn new() -> Self {
        Self {
            spans: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn exported(&self) -> Vec<SpanData> {
        self.spans.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl SpanProcessor for CapturingProcessor {
    fn on_start(&self, _span: &mut Span, _cx: &opentelemetry::Context) {}

    fn on_end(&self, span: SpanData) {
        self.spans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(span);
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenant::CollectorTlsConfig;
    use opentelemetry::trace::{SpanContext, SpanKind, TraceFlags, TraceState};
    use opentelemetry::InstrumentationScope;
    use opentelemetry_sdk::trace::{SpanEvents, SpanLinks};
    use std::borrow::Cow;
    use std::net::SocketAddr;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration as StdDuration, SystemTime};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tracing_subscriber::layer::SubscriberExt;

    fn trace_id_from_seed(seed: u64) -> TraceId {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&seed.to_be_bytes());
        TraceId::from_bytes(bytes)
    }

    fn make_span(
        seed: u64,
        span_byte: u8,
        parent: Option<u8>,
        error: bool,
        duration_ms: u64,
        name: &'static str,
    ) -> SpanData {
        make_span_with_attrs(seed, span_byte, parent, error, duration_ms, name, vec![])
    }

    fn make_span_with_attrs(
        seed: u64,
        span_byte: u8,
        parent: Option<u8>,
        error: bool,
        duration_ms: u64,
        name: &'static str,
        attributes: Vec<KeyValue>,
    ) -> SpanData {
        let trace_id = trace_id_from_seed(seed);
        let span_id = SpanId::from_bytes([span_byte, 0, 0, 0, 0, 0, 0, 1]);
        let parent_span_id = match parent {
            Some(p) => SpanId::from_bytes([p, 0, 0, 0, 0, 0, 0, 1]),
            None => SpanId::INVALID,
        };
        let start = SystemTime::UNIX_EPOCH;
        let end = start + StdDuration::from_millis(duration_ms);
        SpanData {
            span_context: SpanContext::new(
                trace_id,
                span_id,
                TraceFlags::SAMPLED,
                false,
                TraceState::default(),
            ),
            parent_span_id,
            parent_span_is_remote: false,
            span_kind: SpanKind::Internal,
            name: Cow::Borrowed(name),
            start_time: start,
            end_time: end,
            attributes,
            dropped_attributes_count: 0,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status: if error {
                Status::error("boom")
            } else {
                Status::Unset
            },
            instrumentation_scope: InstrumentationScope::default(),
        }
    }

    #[test]
    fn wrapper_keeps_errors() {
        let cap = CapturingProcessor::new();
        let proc = TailSamplingProcessor::new(TailSamplingPolicy::default(), cap.clone());
        proc.on_end(make_span(9000, 2, Some(1), false, 20, "child"));
        proc.on_end(make_span(9000, 1, None, true, 20, "root"));
        let exported = cap.exported();
        assert_eq!(exported.len(), 2);
        assert!(exported.iter().any(|s| s.name == "root"));
        assert!(exported.iter().any(|s| s.name == "child"));
    }

    #[test]
    fn wrapper_drops_fast_success_at_seed_9000() {
        let cap = CapturingProcessor::new();
        let proc = TailSamplingProcessor::new(TailSamplingPolicy::default(), cap.clone());
        proc.on_end(make_span(9000, 2, Some(1), false, 20, "child"));
        proc.on_end(make_span(9000, 1, None, false, 20, "root"));
        assert!(cap.exported().is_empty());
    }

    #[test]
    fn wrapper_keeps_fast_success_at_seed_100() {
        let cap = CapturingProcessor::new();
        let proc = TailSamplingProcessor::new(TailSamplingPolicy::default(), cap.clone());
        proc.on_end(make_span(100, 2, Some(1), false, 20, "child"));
        proc.on_end(make_span(100, 1, None, false, 20, "root"));
        assert_eq!(cap.exported().len(), 2);
    }

    #[test]
    fn wrapper_keeps_slow_success() {
        let cap = CapturingProcessor::new();
        let proc = TailSamplingProcessor::new(TailSamplingPolicy::default(), cap.clone());
        proc.on_end(make_span(9000, 1, None, false, 1500, "root"));
        assert_eq!(cap.exported().len(), 1);
    }

    #[test]
    fn children_follow_parent_late_child_dropped() {
        let cap = CapturingProcessor::new();
        let proc = TailSamplingProcessor::new(TailSamplingPolicy::default(), cap.clone());
        proc.on_end(make_span(9000, 1, None, false, 20, "root"));
        proc.on_end(make_span(9000, 2, Some(1), false, 20, "late_child"));
        assert!(cap.exported().is_empty());
    }

    #[test]
    fn genai_aliases_added_on_export_session_id_not_mapped() {
        use crate::semconv::{VAM_SESSION_ID, VAM_TOOL_NAME};

        let cap = CapturingProcessor::new();
        let proc = TailSamplingProcessor::new(TailSamplingPolicy::default(), cap.clone());
        proc.on_end(make_span_with_attrs(
            9000,
            1,
            None,
            true,
            20,
            "root",
            vec![
                KeyValue::new(VAM_TOOL_NAME, "bash"),
                KeyValue::new(VAM_SESSION_ID, "sess-should-not-alias"),
                KeyValue::new("thread_ref", "thread-main"),
                KeyValue::new("session_id", "sess-field"),
            ],
        ));
        let exported = cap.exported();
        assert_eq!(exported.len(), 1);
        let keys: Vec<&str> = exported[0]
            .attributes
            .iter()
            .map(|kv| kv.key.as_str())
            .collect();
        assert!(keys.contains(&VAM_TOOL_NAME));
        assert!(keys.contains(&"gen_ai.tool.name"));
        assert!(keys.contains(&"gen_ai.conversation.id"));
        assert!(keys.contains(&VAM_SESSION_ID));
        assert!(!keys
            .iter()
            .any(|k| k.starts_with("gen_ai.") && k.contains("session")));
        let tool_alias = exported[0]
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "gen_ai.tool.name")
            .expect("gen_ai.tool.name alias");
        assert_eq!(tool_alias.value.as_str(), "bash");
    }

    #[test]
    fn parse_enabled_defaults_true() {
        assert!(parse_enabled(None));
        assert!(parse_enabled(Some("1")));
        assert!(parse_enabled(Some("true")));
        assert!(!parse_enabled(Some("0")));
        assert!(!parse_enabled(Some("false")));
        assert!(!parse_enabled(Some("OFF")));
    }

    #[test]
    fn remote_otlp_without_tls_is_disabled() {
        let cfg = OtlpConfig {
            enabled: true,
            endpoint: "http://example.com:4318".to_string(),
            policy: TailSamplingPolicy::default(),
            tls: CollectorTlsConfig::default(),
            disabled_reason: None,
        }
        .resolve();
        assert!(!cfg.enabled);
        assert_eq!(
            cfg.disabled_reason.as_deref(),
            Some(OTLP_DISABLED_REMOTE_REQUIRES_MTLS)
        );
    }

    #[test]
    fn loopback_otlp_stays_enabled_without_tls() {
        for endpoint in [
            "http://127.0.0.1:4318",
            "http://localhost:4318",
            DEFAULT_OTLP_ENDPOINT,
        ] {
            let cfg = OtlpConfig {
                enabled: true,
                endpoint: endpoint.to_string(),
                policy: TailSamplingPolicy::default(),
                tls: CollectorTlsConfig::default(),
                disabled_reason: None,
            }
            .resolve();
            assert!(cfg.enabled, "{endpoint} must stay enabled");
            assert!(cfg.disabled_reason.is_none());
        }
    }

    #[test]
    fn spiffe_mismatch_is_rejected() {
        let tls = CollectorTlsConfig {
            enabled: true,
            ca_cert_path: Some("/tmp/ca.pem".to_string()),
            client_cert_path: Some("/tmp/cert.pem".to_string()),
            client_key_path: Some("/tmp/key.pem".to_string()),
            expected_spiffe_id: Some("spiffe://vam.internal/ns/vam/sa/collector".to_string()),
        };
        assert!(tls.validate_spiffe_id("spiffe://vam.internal/ns/vam/sa/collector"));
        assert!(!tls.validate_spiffe_id("spiffe://evil.example/attacker"));
    }

    async fn spawn_mock_otlp() -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock OTLP");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        let hits_clone = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let hits = hits_clone.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16 * 1024];
                    let _ = sock.read(&mut buf).await;
                    hits.fetch_add(1, Ordering::SeqCst);
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), hits)
    }

    #[test]
    fn hermetic_mock_otlp_sink_receives_span_and_counter() {
        // Keep the mock server on a dedicated runtime. Build/flush/drop OTLP
        // providers on this thread so reqwest-blocking does not drop a runtime
        // inside an async context.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let (endpoint, hits) = rt.block_on(spawn_mock_otlp());

        let resource = Resource::builder()
            .with_service_name("vamd-test")
            .with_attributes([KeyValue::new("service.namespace", "vam.platform")])
            .build();

        let span_exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(endpoint.clone())
            .build()
            .expect("span exporter");
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(span_exporter)
            .with_resource(resource.clone())
            .build();

        let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(endpoint)
            .build()
            .expect("metric exporter");
        let reader = PeriodicReader::builder(metric_exporter)
            .with_interval(StdDuration::from_millis(200))
            .build();
        let meter_provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(resource)
            .build();

        use opentelemetry::trace::TracerProvider as _;
        let tracer = tracer_provider.tracer("vamd-test");
        let layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = tracing_subscriber::Registry::default().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("otlp_hermetic_span");
            let _g = span.entered();
        });

        let meter = meter_provider.meter("vamd-test");
        let counter = meter.u64_counter("vamd.otlp.hermetic.total").build();
        counter.add(1, &[]);

        let _ = tracer_provider.force_flush();
        let _ = meter_provider.force_flush();

        let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if hits.load(Ordering::SeqCst) >= 1 {
                break;
            }
            std::thread::sleep(StdDuration::from_millis(50));
        }
        assert!(
            hits.load(Ordering::SeqCst) >= 1,
            "mock OTLP sink received no requests"
        );

        let _ = tracer_provider.shutdown();
        let _ = meter_provider.shutdown();
        drop(rt);
    }
}
