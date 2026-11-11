use crate::error::VamTelemetryError;
use crate::semconv::validate_metric_label;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

/// Explicit histogram bucket upper bounds (milliseconds / bytes / generic units).
/// The final implicit `+Inf` bucket is stored at index `HISTOGRAM_BOUNDARIES.len()`.
pub const HISTOGRAM_BOUNDARIES: [f64; 12] = [
    1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0,
];

/// Inclusive `+Inf` bucket makes this one longer than `HISTOGRAM_BOUNDARIES`.
pub const HISTOGRAM_BUCKET_COUNT: usize = HISTOGRAM_BOUNDARIES.len() + 1;

/// Fixed-size histogram: bucket counts + sum + count. Never grows a sample `Vec`.
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramSnapshot {
    pub buckets: [u64; HISTOGRAM_BUCKET_COUNT],
    pub sum: f64,
    pub count: u64,
}

impl Default for HistogramSnapshot {
    fn default() -> Self {
        Self {
            buckets: [0; HISTOGRAM_BUCKET_COUNT],
            sum: 0.0,
            count: 0,
        }
    }
}

impl HistogramSnapshot {
    pub fn record(&mut self, sample: f64) {
        self.sum += sample;
        self.count = self.count.saturating_add(1);
        for (i, bound) in HISTOGRAM_BOUNDARIES.iter().enumerate() {
            if sample <= *bound {
                self.buckets[i] = self.buckets[i].saturating_add(1);
                return;
            }
        }
        self.buckets[HISTOGRAM_BUCKET_COUNT - 1] =
            self.buckets[HISTOGRAM_BUCKET_COUNT - 1].saturating_add(1);
    }
}

/// Optional OTLP sidecar for histogram dual-write (samples cannot be replayed from buckets).
pub(crate) trait MetricsBridge: Send + Sync {
    fn record_histogram(&self, name: &str, sample: f64, labels: &[(&str, &str)]);
}

static METRICS_BRIDGE: OnceLock<Arc<dyn MetricsBridge>> = OnceLock::new();

pub(crate) fn install_metrics_bridge(bridge: Arc<dyn MetricsBridge>) {
    let _ = METRICS_BRIDGE.set(bridge);
}

fn bridge() -> Option<&'static Arc<dyn MetricsBridge>> {
    METRICS_BRIDGE.get()
}

pub const METRIC_POOL_OCCUPANCY: &str = "vamd.pool.occupancy";
pub const METRIC_ADMISSION_REQUESTS_TOTAL: &str = "vamd.admission.requests.total";
pub const METRIC_QUEUE_DELAY_MS: &str = "vamd.queue.delay_ms";
pub const METRIC_ROUTER_BACKPRESSURE_TOTAL: &str = "vamd.router.backpressure_events.total";
pub const METRIC_STORAGE_SYNC_MS: &str = "vamd.storage.sync.duration_ms";
pub const METRIC_SECURITY_DENIED_TOTAL: &str = "vamd.security.denied_actions.total";
pub const METRIC_HSP_INVOKE_DURATION_MS: &str = "vamd.hsp.invoke.duration_ms";
pub const METRIC_HSP_HOOK_WAVE_DURATION_MS: &str = "vamd.hsp.hook_wave.duration_ms";
pub const METRIC_HSP_INVOKE_ERRORS_TOTAL: &str = "vamd.hsp.invoke.errors.total";
pub const METRIC_HSP_HOOK_FAILURES_TOTAL: &str = "vamd.hsp.hook.failures.total";

// Phase 5 Hypervisor Metrics (Scraped from Firecracker JSON)
pub const METRIC_HYPERVISOR_VCPU_EXITS_TOTAL: &str = "vam_hypervisor_vcpu_exits_total";
pub const METRIC_HYPERVISOR_BLOCK_READ_BYTES: &str = "vam_hypervisor_block_read_bytes_total";
pub const METRIC_HYPERVISOR_BLOCK_WRITE_BYTES: &str = "vam_hypervisor_block_write_bytes_total";
pub const METRIC_HYPERVISOR_NET_RX_BYTES: &str = "vam_hypervisor_net_rx_bytes_total";
pub const METRIC_HYPERVISOR_NET_TX_BYTES: &str = "vam_hypervisor_net_tx_bytes_total";
pub const METRIC_HYPERVISOR_VSOCK_RX_BYTES: &str = "vam_hypervisor_vsock_rx_bytes_total";
pub const METRIC_HYPERVISOR_VSOCK_TX_BYTES: &str = "vam_hypervisor_vsock_tx_bytes_total";

// Phase 5 Host cgroup v2 Resource Attribution
pub const METRIC_CGROUP_CPU_USAGE_US: &str = "vam_cgroup_cpu_usage_us_total";
pub const METRIC_CGROUP_MEMORY_CURRENT_BYTES: &str = "vam_cgroup_memory_current_bytes";
pub const METRIC_CGROUP_MEMORY_PEAK_BYTES: &str = "vam_cgroup_memory_peak_bytes";
pub const METRIC_CGROUP_THROTTLED_US: &str = "vam_cgroup_cpu_throttled_us_total";

// Phase 5 Network Flow Metrics & Security Lane Denials
pub const METRIC_NETWORK_EGRESS_BYTES_TOTAL: &str = "vam_network_egress_bytes_total";
pub const METRIC_NETWORK_INGRESS_BYTES_TOTAL: &str = "vam_network_ingress_bytes_total";
pub const METRIC_NETWORK_PACKETS_TOTAL: &str = "vam_network_packets_total";
pub const METRIC_NETWORK_DROPPED_PACKETS_TOTAL: &str = "vam_network_dropped_packets_total";
pub const METRIC_NETWORK_DNS_QUERIES_TOTAL: &str = "vam_network_dns_queries_total";
pub const METRIC_NETWORK_SHAPING_BACKPRESSURE_EVENTS_TOTAL: &str =
    "vam_network_shaping_backpressure_events_total";
pub const METRIC_NETWORK_POLICY_COMPILE_MS: &str = "vam_network_policy_compile_duration_ms";
pub const METRIC_NETWORK_POLICY_ACTIVATION_MS: &str = "vam_network_policy_activation_duration_ms";

// Phase 5 Drive9 Storage & Materialization Caching
pub const METRIC_DRIVE9_CHUNK_COUNT: &str = "vam_storage_drive9_chunk_count_total";
pub const METRIC_DRIVE9_CHUNK_BYTES: &str = "vam_storage_drive9_chunk_bytes_total";
pub const METRIC_DRIVE9_CACHE_HITS_TOTAL: &str = "vam_storage_drive9_cache_hits_total";
pub const METRIC_DRIVE9_CACHE_MISSES_TOTAL: &str = "vam_storage_drive9_cache_misses_total";

/// Process-wide SLO breach counter. Label `tier` only (`claim`|`restore`|`cold`). No session_id (T20).
pub const METRIC_SLO_BREACHES_TOTAL: &str = "vamd.slo.breaches.total";
/// Process-wide SLO observation counter. Label `tier` only (T20).
pub const METRIC_SLO_OBSERVATIONS_TOTAL: &str = "vamd.slo.observations.total";
/// Guardrail signals (deny spike / same-tool loop). Label `status` only (T20).
pub const METRIC_GUARDRAIL_SIGNALS_TOTAL: &str = "vamd.guardrail.signals.total";

pub const DOCUMENTED_METRICS: &[&str] = &[
    METRIC_POOL_OCCUPANCY,
    METRIC_ADMISSION_REQUESTS_TOTAL,
    METRIC_QUEUE_DELAY_MS,
    METRIC_ROUTER_BACKPRESSURE_TOTAL,
    METRIC_STORAGE_SYNC_MS,
    METRIC_SECURITY_DENIED_TOTAL,
    METRIC_HSP_INVOKE_DURATION_MS,
    METRIC_HSP_HOOK_WAVE_DURATION_MS,
    METRIC_HSP_INVOKE_ERRORS_TOTAL,
    METRIC_HSP_HOOK_FAILURES_TOTAL,
    METRIC_HYPERVISOR_VCPU_EXITS_TOTAL,
    METRIC_HYPERVISOR_BLOCK_READ_BYTES,
    METRIC_HYPERVISOR_BLOCK_WRITE_BYTES,
    METRIC_HYPERVISOR_NET_RX_BYTES,
    METRIC_HYPERVISOR_NET_TX_BYTES,
    METRIC_HYPERVISOR_VSOCK_RX_BYTES,
    METRIC_HYPERVISOR_VSOCK_TX_BYTES,
    METRIC_CGROUP_CPU_USAGE_US,
    METRIC_CGROUP_MEMORY_CURRENT_BYTES,
    METRIC_CGROUP_MEMORY_PEAK_BYTES,
    METRIC_CGROUP_THROTTLED_US,
    METRIC_NETWORK_EGRESS_BYTES_TOTAL,
    METRIC_NETWORK_INGRESS_BYTES_TOTAL,
    METRIC_NETWORK_PACKETS_TOTAL,
    METRIC_NETWORK_DROPPED_PACKETS_TOTAL,
    METRIC_NETWORK_DNS_QUERIES_TOTAL,
    METRIC_NETWORK_SHAPING_BACKPRESSURE_EVENTS_TOTAL,
    METRIC_NETWORK_POLICY_COMPILE_MS,
    METRIC_NETWORK_POLICY_ACTIVATION_MS,
    METRIC_DRIVE9_CHUNK_COUNT,
    METRIC_DRIVE9_CHUNK_BYTES,
    METRIC_DRIVE9_CACHE_HITS_TOTAL,
    METRIC_DRIVE9_CACHE_MISSES_TOTAL,
    METRIC_SLO_BREACHES_TOTAL,
    METRIC_SLO_OBSERVATIONS_TOTAL,
    METRIC_GUARDRAIL_SIGNALS_TOTAL,
];

pub type LabelList = Vec<(String, String)>;

/// Low-overhead, thread-safe in-memory metrics registry for `vamd`.
/// Enforces the low-cardinality dimension policy (Decision T20) at ingestion time.
#[derive(Debug, Default)]
pub struct VamMetricsRegistry {
    counters: RwLock<HashMap<(String, LabelList), u64>>,
    gauges: RwLock<HashMap<(String, LabelList), i64>>,
    histograms: RwLock<HashMap<(String, LabelList), HistogramSnapshot>>,
}

static GLOBAL_REGISTRY: OnceLock<Arc<VamMetricsRegistry>> = OnceLock::new();

impl VamMetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn global() -> Arc<Self> {
        GLOBAL_REGISTRY
            .get_or_init(|| Arc::new(Self::new()))
            .clone()
    }

    fn validate_labels(&self, labels: &[(&str, &str)]) -> Result<LabelList, VamTelemetryError> {
        let mut sorted = Vec::with_capacity(labels.len());
        for (key, val) in labels {
            validate_metric_label(key, val)?;
            sorted.push((key.to_string(), val.to_string()));
        }
        sorted.sort();
        Ok(sorted)
    }

    /// Record an increment to a monotonic counter (e.g. `vamd.admission.requests.total`).
    pub fn record_counter(
        &self,
        name: &str,
        delta: u64,
        labels: &[(&str, &str)],
    ) -> Result<(), VamTelemetryError> {
        let sorted_labels = self.validate_labels(labels)?;
        let mut counters = self
            .counters
            .write()
            .map_err(|e| VamTelemetryError::InvalidLabel {
                key: name.to_string(),
                reason: format!("lock poisoned: {}", e),
            })?;
        let entry = counters
            .entry((name.to_string(), sorted_labels))
            .or_insert(0);
        *entry += delta;
        drop(counters);
        Ok(())
    }

    /// Set an absolute counter value (e.g. cumulative totals from Firecracker scraper).
    pub fn set_counter(
        &self,
        name: &str,
        value: u64,
        labels: &[(&str, &str)],
    ) -> Result<(), VamTelemetryError> {
        let sorted_labels = self.validate_labels(labels)?;
        let mut counters = self
            .counters
            .write()
            .map_err(|e| VamTelemetryError::InvalidLabel {
                key: name.to_string(),
                reason: format!("lock poisoned: {}", e),
            })?;
        counters.insert((name.to_string(), sorted_labels), value);
        drop(counters);
        Ok(())
    }

    /// Set an instantaneous gauge value (e.g. `vamd.pool.occupancy`).
    pub fn set_gauge(
        &self,
        name: &str,
        value: i64,
        labels: &[(&str, &str)],
    ) -> Result<(), VamTelemetryError> {
        let sorted_labels = self.validate_labels(labels)?;
        let mut gauges = self
            .gauges
            .write()
            .map_err(|e| VamTelemetryError::InvalidLabel {
                key: name.to_string(),
                reason: format!("lock poisoned: {}", e),
            })?;
        gauges.insert((name.to_string(), sorted_labels), value);
        drop(gauges);
        Ok(())
    }

    /// Record a duration or size sample to a histogram (e.g. `vamd.queue.delay_ms`).
    pub fn record_histogram(
        &self,
        name: &str,
        sample: f64,
        labels: &[(&str, &str)],
    ) -> Result<(), VamTelemetryError> {
        let sorted_labels = self.validate_labels(labels)?;
        let mut histograms =
            self.histograms
                .write()
                .map_err(|e| VamTelemetryError::InvalidLabel {
                    key: name.to_string(),
                    reason: format!("lock poisoned: {}", e),
                })?;
        let entry = histograms
            .entry((name.to_string(), sorted_labels))
            .or_default();
        entry.record(sample);
        drop(histograms);
        if let Some(bridge) = bridge() {
            bridge.record_histogram(name, sample, labels);
        }
        Ok(())
    }

    pub fn get_counter(&self, name: &str, labels: &[(&str, &str)]) -> Option<u64> {
        let sorted_labels = self.validate_labels(labels).ok()?;
        let counters = self.counters.read().ok()?;
        counters.get(&(name.to_string(), sorted_labels)).copied()
    }

    pub fn get_gauge(&self, name: &str, labels: &[(&str, &str)]) -> Option<i64> {
        let sorted_labels = self.validate_labels(labels).ok()?;
        let gauges = self.gauges.read().ok()?;
        gauges.get(&(name.to_string(), sorted_labels)).copied()
    }

    pub fn get_histogram(&self, name: &str, labels: &[(&str, &str)]) -> Option<HistogramSnapshot> {
        let sorted_labels = self.validate_labels(labels).ok()?;
        let histograms = self.histograms.read().ok()?;
        histograms.get(&(name.to_string(), sorted_labels)).cloned()
    }

    /// Snapshot of every counter currently held (for the OTLP periodic bridge).
    pub fn snapshot_counters(&self) -> Vec<(String, LabelList, u64)> {
        let Ok(counters) = self.counters.read() else {
            return Vec::new();
        };
        counters
            .iter()
            .map(|((name, labels), value)| (name.clone(), labels.clone(), *value))
            .collect()
    }

    /// Snapshot of every gauge currently held (for the OTLP periodic bridge).
    pub fn snapshot_gauges(&self) -> Vec<(String, LabelList, i64)> {
        let Ok(gauges) = self.gauges.read() else {
            return Vec::new();
        };
        gauges
            .iter()
            .map(|((name, labels), value)| (name.clone(), labels.clone(), *value))
            .collect()
    }

    /// Snapshot of every histogram currently held (for the OTLP periodic bridge).
    pub fn snapshot_histograms(&self) -> Vec<(String, LabelList, HistogramSnapshot)> {
        let Ok(histograms) = self.histograms.read() else {
            return Vec::new();
        };
        histograms
            .iter()
            .map(|((name, labels), value)| (name.clone(), labels.clone(), value.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_metrics_recording() {
        let registry = VamMetricsRegistry::new();

        // Valid Counter
        assert!(registry
            .record_counter(
                METRIC_ADMISSION_REQUESTS_TOTAL,
                1,
                &[
                    ("admission_status", "allowed"),
                    ("network_mode", "restricted_direct")
                ]
            )
            .is_ok());

        assert_eq!(
            registry.get_counter(
                METRIC_ADMISSION_REQUESTS_TOTAL,
                &[
                    ("network_mode", "restricted_direct"),
                    ("admission_status", "allowed")
                ]
            ),
            Some(1)
        );

        // Valid Gauge
        assert!(registry
            .set_gauge(
                METRIC_POOL_OCCUPANCY,
                4,
                &[("network_mode", "host_loopback")]
            )
            .is_ok());

        assert_eq!(
            registry.get_gauge(METRIC_POOL_OCCUPANCY, &[("network_mode", "host_loopback")]),
            Some(4)
        );

        // Valid Histogram — 12.5 lands in the 25ms bucket (index 3)
        assert!(registry
            .record_histogram(METRIC_QUEUE_DELAY_MS, 12.5, &[("model_billed", "gpt-4o")])
            .is_ok());

        let snap = registry
            .get_histogram(METRIC_QUEUE_DELAY_MS, &[("model_billed", "gpt-4o")])
            .expect("histogram sample");
        assert_eq!(snap.count, 1);
        assert_eq!(snap.sum, 12.5);
        assert_eq!(snap.buckets.len(), HISTOGRAM_BUCKET_COUNT);
        assert_eq!(snap.buckets[3], 1);
    }

    #[test]
    fn histogram_does_not_grow_a_vec() {
        let registry = VamMetricsRegistry::new();
        for i in 0..10_000 {
            assert!(registry
                .record_histogram(METRIC_QUEUE_DELAY_MS, i as f64, &[])
                .is_ok());
        }
        let snap = registry
            .get_histogram(METRIC_QUEUE_DELAY_MS, &[])
            .expect("histogram after many samples");
        assert_eq!(snap.count, 10_000);
        assert_eq!(snap.buckets.len(), HISTOGRAM_BUCKET_COUNT);
        let bucket_sum: u64 = snap.buckets.iter().sum();
        assert_eq!(bucket_sum, 10_000);
        // Storage is a fixed array; recording more samples must not add buckets.
        assert_eq!(
            std::mem::size_of_val(&snap.buckets),
            HISTOGRAM_BUCKET_COUNT * 8
        );
    }

    #[test]
    fn test_disallowed_cardinality_label_rejected() {
        let registry = VamMetricsRegistry::new();

        // High-cardinality session ID or prompt must be rejected
        let res = registry.record_counter(
            METRIC_ADMISSION_REQUESTS_TOTAL,
            1,
            &[("session_id", "session-123456")],
        );
        assert!(res.is_err());
        match res.unwrap_err() {
            VamTelemetryError::InvalidLabel { key, .. } => assert_eq!(key, "session_id"),
            other => panic!("expected InvalidLabel, got {:?}", other),
        }

        let res_prompt =
            registry.record_histogram(METRIC_QUEUE_DELAY_MS, 5.0, &[("prompt", "Hello AI!")]);
        assert!(res_prompt.is_err());
    }
}
