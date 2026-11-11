//! Service Level Objective (SLO) tracking, latency tier evaluation, and alert generation (Phase 7.2).

use crate::metrics::{
    VamMetricsRegistry, METRIC_SLO_BREACHES_TOTAL, METRIC_SLO_OBSERVATIONS_TOTAL,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static GLOBAL_SLO: OnceLock<SloEvaluator> = OnceLock::new();

/// Documented VAM session lifecycle latency tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SloTier {
    /// Tier 1: Warm pool session claim (<10ms target).
    Claim,
    /// Tier 2: MicroVM restore from snapshot (<50ms target).
    Restore,
    /// Tier 3: Cold microVM boot with Drive9 ext4 rootfs (<250ms target).
    Cold,
}

impl SloTier {
    /// Latency ceiling target in milliseconds.
    pub const fn target_ms(&self) -> u64 {
        match self {
            SloTier::Claim => 10,
            SloTier::Restore => 50,
            SloTier::Cold => 250,
        }
    }

    pub const fn as_str(&self) -> &'static str {
        match self {
            SloTier::Claim => "claim",
            SloTier::Restore => "restore",
            SloTier::Cold => "cold",
        }
    }
}

/// Alert emitted when an operation exceeds its documented SLO latency tier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SloAlert {
    pub tier: SloTier,
    pub observed_ms: u64,
    pub target_ms: u64,
    pub session_id: String,
    pub timestamp_unix_ms: i64,
    pub message: String,
}

/// Capacity and concurrency state for orchestrator SLO tracking.
#[derive(Debug, Default)]
pub struct CapacityMetrics {
    pub active_sessions: AtomicU64,
    pub warm_pool_size: AtomicU64,
    pub admission_queue_len: AtomicU64,
}

/// Thread-safe SLO latency tier evaluator and breach detector.
#[derive(Debug, Default)]
pub struct SloEvaluator {
    total_evaluations: Mutex<HashMap<SloTier, u64>>,
    breach_count: Mutex<HashMap<SloTier, u64>>,
    last_breach: Mutex<HashMap<SloTier, Option<SloAlert>>>,
    pub capacity: CapacityMetrics,
}

impl SloEvaluator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process-wide evaluator (same style as [`VamMetricsRegistry::global`]).
    pub fn global() -> &'static Self {
        GLOBAL_SLO.get_or_init(Self::new)
    }

    /// Record an observation, increment registry counters (label `tier` only; T20), warn on breach.
    pub fn report_live(
        &self,
        registry: &VamMetricsRegistry,
        tier: SloTier,
        observed_ms: u64,
        session_id: &str,
    ) -> Option<SloAlert> {
        let _ =
            registry.record_counter(METRIC_SLO_OBSERVATIONS_TOTAL, 1, &[("tier", tier.as_str())]);
        let alert = self.record_observation(tier, observed_ms, session_id);
        if let Some(ref a) = alert {
            tracing::warn!(
                target: "vam.telemetry.slo",
                tier = a.tier.as_str(),
                observed_ms = a.observed_ms,
                target_ms = a.target_ms,
                session_id,
                "SLO latency breached"
            );
            let _ =
                registry.record_counter(METRIC_SLO_BREACHES_TOTAL, 1, &[("tier", a.tier.as_str())]);
        }
        alert
    }

    /// Evaluates an observed latency against the SLO tier target.
    /// If the observed duration breaches the target ceiling, an `SloAlert` is generated.
    pub fn record_observation(
        &self,
        tier: SloTier,
        observed_ms: u64,
        session_id: &str,
    ) -> Option<SloAlert> {
        let target_ms = tier.target_ms();
        let is_breach = observed_ms > target_ms;

        // 1. Update total observation count
        if let Ok(mut totals) = self.total_evaluations.lock() {
            *totals.entry(tier).or_insert(0) += 1;
        }

        if is_breach {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);

            let alert = SloAlert {
                tier,
                observed_ms,
                target_ms,
                session_id: session_id.to_string(),
                timestamp_unix_ms: now_ms,
                message: format!(
                    "SLO target breached for {} session '{}': observed {}ms exceeds target {}ms",
                    tier.as_str(),
                    session_id,
                    observed_ms,
                    target_ms
                ),
            };

            // 2. Update breach count & last breach record
            if let Ok(mut breaches) = self.breach_count.lock() {
                *breaches.entry(tier).or_insert(0) += 1;
            }
            if let Ok(mut last) = self.last_breach.lock() {
                last.insert(tier, Some(alert.clone()));
            }

            Some(alert)
        } else {
            None
        }
    }

    /// Computes current adherence ratio (0.0 to 1.0) for a given tier.
    pub fn adherence_ratio(&self, tier: SloTier) -> f64 {
        let total = self
            .total_evaluations
            .lock()
            .ok()
            .and_then(|m| m.get(&tier).copied())
            .unwrap_or(0);

        if total == 0 {
            return 1.0;
        }

        let breaches = self
            .breach_count
            .lock()
            .ok()
            .and_then(|m| m.get(&tier).copied())
            .unwrap_or(0);

        (total.saturating_sub(breaches) as f64) / (total as f64)
    }

    pub fn total_observations(&self, tier: SloTier) -> u64 {
        self.total_evaluations
            .lock()
            .ok()
            .and_then(|m| m.get(&tier).copied())
            .unwrap_or(0)
    }

    pub fn total_breaches(&self, tier: SloTier) -> u64 {
        self.breach_count
            .lock()
            .ok()
            .and_then(|m| m.get(&tier).copied())
            .unwrap_or(0)
    }

    pub fn last_breach_alert(&self, tier: SloTier) -> Option<SloAlert> {
        self.last_breach
            .lock()
            .ok()
            .and_then(|m| m.get(&tier).cloned().flatten())
    }

    // Capacity gauges helpers
    pub fn increment_active_sessions(&self) -> u64 {
        self.capacity.active_sessions.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn decrement_active_sessions(&self) -> u64 {
        self.capacity
            .active_sessions
            .fetch_sub(1, Ordering::SeqCst)
            .saturating_sub(1)
    }

    pub fn set_warm_pool_size(&self, count: u64) {
        self.capacity.warm_pool_size.store(count, Ordering::SeqCst);
    }

    pub fn set_admission_queue_len(&self, count: u64) {
        self.capacity
            .admission_queue_len
            .store(count, Ordering::SeqCst);
    }
}

/// Restore / Cold (and Claim if a real clock exists) → global evaluator + registry.
pub fn report_slo_observation(
    tier: SloTier,
    observed_ms: u64,
    session_id: &str,
) -> Option<SloAlert> {
    SloEvaluator::global().report_live(&VamMetricsRegistry::global(), tier, observed_ms, session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{
        VamMetricsRegistry, METRIC_SLO_BREACHES_TOTAL, METRIC_SLO_OBSERVATIONS_TOTAL,
    };

    #[test]
    fn test_slo_tier_targets() {
        assert_eq!(SloTier::Claim.target_ms(), 10);
        assert_eq!(SloTier::Restore.target_ms(), 50);
        assert_eq!(SloTier::Cold.target_ms(), 250);
    }

    #[test]
    fn test_slo_adherence_and_alert_generation() {
        let evaluator = SloEvaluator::new();

        // 1. Within target: Claim at 5ms
        let alert = evaluator.record_observation(SloTier::Claim, 5, "sess-1");
        assert!(alert.is_none());
        assert_eq!(evaluator.adherence_ratio(SloTier::Claim), 1.0);

        // 2. Breach: Claim at 15ms (>10ms)
        let alert = evaluator.record_observation(SloTier::Claim, 15, "sess-2");
        assert!(alert.is_some());
        let a = alert.unwrap();
        assert_eq!(a.tier, SloTier::Claim);
        assert_eq!(a.observed_ms, 15);
        assert_eq!(a.target_ms, 10);
        assert_eq!(a.session_id, "sess-2");
        assert_eq!(evaluator.total_breaches(SloTier::Claim), 1);
        assert_eq!(evaluator.total_observations(SloTier::Claim), 2);
        assert_eq!(evaluator.adherence_ratio(SloTier::Claim), 0.5);

        // 3. Restore: 45ms (ok), 60ms (breach)
        assert!(evaluator
            .record_observation(SloTier::Restore, 45, "sess-3")
            .is_none());
        assert!(evaluator
            .record_observation(SloTier::Restore, 60, "sess-4")
            .is_some());
        assert_eq!(evaluator.total_breaches(SloTier::Restore), 1);

        // 4. Cold: 200ms (ok)
        assert!(evaluator
            .record_observation(SloTier::Cold, 200, "sess-5")
            .is_none());
        assert_eq!(evaluator.adherence_ratio(SloTier::Cold), 1.0);
    }

    #[test]
    fn test_capacity_metrics_gauges() {
        let evaluator = SloEvaluator::new();
        assert_eq!(evaluator.increment_active_sessions(), 1);
        assert_eq!(evaluator.increment_active_sessions(), 2);
        assert_eq!(evaluator.decrement_active_sessions(), 1);

        evaluator.set_warm_pool_size(4);
        assert_eq!(evaluator.capacity.warm_pool_size.load(Ordering::SeqCst), 4);

        evaluator.set_admission_queue_len(12);
        assert_eq!(
            evaluator
                .capacity
                .admission_queue_len
                .load(Ordering::SeqCst),
            12
        );
    }

    #[test]
    fn shared_evaluator_restore_counters_persist() {
        let evaluator = SloEvaluator::new();
        let registry = VamMetricsRegistry::new();

        assert!(evaluator
            .report_live(&registry, SloTier::Restore, 60, "sess-a")
            .is_some());
        assert!(evaluator
            .report_live(&registry, SloTier::Restore, 80, "sess-b")
            .is_some());

        assert_eq!(evaluator.total_observations(SloTier::Restore), 2);
        assert_eq!(evaluator.total_breaches(SloTier::Restore), 2);
        assert_eq!(
            registry.get_counter(METRIC_SLO_BREACHES_TOTAL, &[("tier", "restore")]),
            Some(2)
        );
        assert_eq!(
            registry.get_counter(METRIC_SLO_OBSERVATIONS_TOTAL, &[("tier", "restore")]),
            Some(2)
        );
    }
}
