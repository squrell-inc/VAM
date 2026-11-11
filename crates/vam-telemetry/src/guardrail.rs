//! Process-wide guardrail signals (denial spike, same-tool loop).
//!
//! Metric labels stay T20: `status` only. Tool name and `session_id` stay on
//! `tracing` fields, never on metric labels. Telemetry reports; it does not
//! enforce spend (T18).

use crate::metrics::{
    VamMetricsRegistry, METRIC_GUARDRAIL_SIGNALS_TOTAL, METRIC_SECURITY_DENIED_TOTAL,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Sliding window used for `status=deny_spike`.
pub const DENY_SPIKE_WINDOW: Duration = Duration::from_secs(60);
/// Denies inside [`DENY_SPIKE_WINDOW`] that trip a spike signal.
pub const DENY_SPIKE_THRESHOLD: u64 = 10;
/// Per-run invokes of one tool name that trip `status=same_tool_loop`.
pub const SAME_TOOL_LOOP_THRESHOLD: u64 = 8;

static GLOBAL: OnceLock<GuardrailSignals> = OnceLock::new();

/// Windowed denial + per-run same-tool counters.
#[derive(Debug)]
pub struct GuardrailSignals {
    window: Duration,
    deny_threshold: u64,
    loop_threshold: u64,
    deny_times: Mutex<VecDeque<Instant>>,
    tools: Mutex<HashMap<String, HashMap<String, u64>>>,
    looped: Mutex<HashSet<String>>,
}

impl Default for GuardrailSignals {
    fn default() -> Self {
        Self::new()
    }
}

impl GuardrailSignals {
    pub fn new() -> Self {
        Self::with_thresholds(
            DENY_SPIKE_WINDOW,
            DENY_SPIKE_THRESHOLD,
            SAME_TOOL_LOOP_THRESHOLD,
        )
    }

    pub fn with_thresholds(window: Duration, deny_threshold: u64, loop_threshold: u64) -> Self {
        Self {
            window,
            deny_threshold,
            loop_threshold,
            deny_times: Mutex::new(VecDeque::new()),
            tools: Mutex::new(HashMap::new()),
            looped: Mutex::new(HashSet::new()),
        }
    }

    pub fn global() -> &'static Self {
        GLOBAL.get_or_init(Self::new)
    }

    /// Cedar DENY: increment denied counter; maybe `deny_spike`.
    pub fn record_deny(&self, registry: &VamMetricsRegistry) {
        let _ = registry.record_counter(METRIC_SECURITY_DENIED_TOTAL, 1, &[("status", "denied")]);
        let now = Instant::now();
        let mut times = self.deny_times.lock().unwrap_or_else(|e| e.into_inner());
        times.push_back(now);
        while times
            .front()
            .map(|t| now.duration_since(*t) > self.window)
            .unwrap_or(false)
        {
            times.pop_front();
        }
        if (times.len() as u64) >= self.deny_threshold {
            times.clear();
            drop(times);
            let _ = registry.record_counter(
                METRIC_GUARDRAIL_SIGNALS_TOTAL,
                1,
                &[("status", "deny_spike")],
            );
        }
    }

    /// Count `vam.v1.tool.invoked` by tool name for one run. Tool name is only
    /// logged, never a metric label (T20).
    pub fn record_tool_invoked(
        &self,
        run_id: &str,
        tool_name: &str,
        registry: &VamMetricsRegistry,
    ) {
        if run_id.is_empty() || tool_name.is_empty() {
            return;
        }
        let mut tools = self.tools.lock().unwrap_or_else(|e| e.into_inner());
        let count = tools
            .entry(run_id.to_string())
            .or_default()
            .entry(tool_name.to_string())
            .or_insert(0);
        *count = count.saturating_add(1);
        let n = *count;
        drop(tools);
        if n > self.loop_threshold {
            let mut looped = self.looped.lock().unwrap_or_else(|e| e.into_inner());
            if looped.insert(run_id.to_string()) {
                tracing::warn!(
                    tool_name = %tool_name,
                    "same-tool loop detected"
                );
                let _ = registry.record_counter(
                    METRIC_GUARDRAIL_SIGNALS_TOTAL,
                    1,
                    &[("status", "same_tool_loop")],
                );
            }
        }
    }
}

/// Process-wide Cedar DENY hook.
pub fn record_security_denied() {
    GuardrailSignals::global().record_deny(&VamMetricsRegistry::global());
}

/// Process-wide tool-invoke hook for same-tool loop detection.
pub fn record_tool_invoked_signal(run_id: &str, tool_name: &str) {
    GuardrailSignals::global().record_tool_invoked(
        run_id,
        tool_name,
        &VamMetricsRegistry::global(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_increments_denied_and_spike() {
        let registry = VamMetricsRegistry::new();
        let signals = GuardrailSignals::with_thresholds(Duration::from_secs(60), 3, 8);
        for _ in 0..2 {
            signals.record_deny(&registry);
        }
        assert_eq!(
            registry.get_counter(METRIC_SECURITY_DENIED_TOTAL, &[("status", "denied")]),
            Some(2)
        );
        assert_eq!(
            registry.get_counter(METRIC_GUARDRAIL_SIGNALS_TOTAL, &[("status", "deny_spike")]),
            None
        );
        signals.record_deny(&registry);
        assert_eq!(
            registry.get_counter(METRIC_SECURITY_DENIED_TOTAL, &[("status", "denied")]),
            Some(3)
        );
        assert_eq!(
            registry.get_counter(METRIC_GUARDRAIL_SIGNALS_TOTAL, &[("status", "deny_spike")]),
            Some(1)
        );
    }

    #[test]
    fn same_tool_loop_fires_once_and_keeps_name_off_labels() {
        let registry = VamMetricsRegistry::new();
        let signals = GuardrailSignals::with_thresholds(Duration::from_secs(60), 10, 2);
        let run_id = "urn:vam:run:loop-test";
        signals.record_tool_invoked(run_id, "bash", &registry);
        signals.record_tool_invoked(run_id, "bash", &registry);
        assert_eq!(
            registry.get_counter(
                METRIC_GUARDRAIL_SIGNALS_TOTAL,
                &[("status", "same_tool_loop")]
            ),
            None
        );
        signals.record_tool_invoked(run_id, "bash", &registry);
        assert_eq!(
            registry.get_counter(
                METRIC_GUARDRAIL_SIGNALS_TOTAL,
                &[("status", "same_tool_loop")]
            ),
            Some(1)
        );
        signals.record_tool_invoked(run_id, "bash", &registry);
        assert_eq!(
            registry.get_counter(
                METRIC_GUARDRAIL_SIGNALS_TOTAL,
                &[("status", "same_tool_loop")]
            ),
            Some(1)
        );
        // T20: tool name / session must not be accepted as labels.
        assert!(registry
            .record_counter(
                METRIC_GUARDRAIL_SIGNALS_TOTAL,
                1,
                &[("status", "same_tool_loop"), ("session_id", "sess-1")]
            )
            .is_err());
    }
}
