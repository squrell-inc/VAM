/// Tail-sampling policy for OTel traces (Decision T18/T20).
///
/// Applied on the host export path so whole traces are kept or dropped together.
#[derive(Debug, Clone)]
pub struct TailSamplingPolicy {
    pub success_sample_rate: f64,
    pub slow_trace_threshold_ms: u64,
}

impl Default for TailSamplingPolicy {
    fn default() -> Self {
        Self {
            success_sample_rate: 0.05,     // 5% default for normal operations
            slow_trace_threshold_ms: 1000, // 1000ms threshold
        }
    }
}

impl TailSamplingPolicy {
    /// Determines whether a trace should be preserved in the export lane.
    /// - All errors are preserved 100%.
    /// - High-latency traces (>= threshold) are preserved 100%.
    /// - Successes are sampled according to `success_sample_rate`.
    pub fn should_sample(&self, is_error: bool, duration_ms: u64, sample_seed: u64) -> bool {
        if is_error {
            return true;
        }
        if duration_ms >= self.slow_trace_threshold_ms {
            return true;
        }
        if self.success_sample_rate >= 1.0 {
            return true;
        }
        if self.success_sample_rate <= 0.0 {
            return false;
        }

        // Pseudo-random deterministic sampling based on sample_seed
        let bucket = (sample_seed % 10000) as f64 / 10000.0;
        bucket < self.success_sample_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tail_sampling_policy() {
        let policy = TailSamplingPolicy::default();

        // 1. Errors always preserved
        assert!(policy.should_sample(true, 50, 9999));

        // 2. High latency always preserved
        assert!(policy.should_sample(false, 1050, 9999));

        // 3. Fast success sampled
        assert!(policy.should_sample(false, 20, 100)); // 100/10000 = 0.01 < 0.05
        assert!(!policy.should_sample(false, 20, 9000)); // 9000/10000 = 0.90 >= 0.05
    }
}
