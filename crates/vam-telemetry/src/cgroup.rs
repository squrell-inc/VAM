use std::fs;
use std::path::Path;
use tracing::info;

use crate::error::VamTelemetryError;
use crate::metrics::*;

/// Parsed cgroup v2 resource usage stats for a microVM session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CgroupResourceUsage {
    pub cpu_usage_us: u64,
    pub cpu_user_us: u64,
    pub cpu_system_us: u64,
    pub cpu_throttled_us: u64,
    pub nr_throttled: u64,
    pub memory_current_bytes: u64,
    pub memory_peak_bytes: u64,
}

impl CgroupResourceUsage {
    /// Returns CPU usage formatted in milliseconds.
    pub fn cpu_usage_ms(&self) -> u64 {
        self.cpu_usage_us / 1000
    }

    /// Returns throttled time formatted in milliseconds.
    pub fn throttled_ms(&self) -> u64 {
        self.cpu_throttled_us / 1000
    }

    /// Returns peak memory formatted in Megabytes.
    pub fn memory_peak_mb(&self) -> f64 {
        (self.memory_peak_bytes as f64) / (1024.0 * 1024.0)
    }
}

/// Parses cgroup v2 `cpu.stat` key-value pairs.
pub fn parse_cpu_stat(content: &str) -> (u64, u64, u64, u64, u64) {
    let mut usage_us = 0;
    let mut user_us = 0;
    let mut system_us = 0;
    let mut nr_throttled = 0;
    let mut throttled_us = 0;

    for line in content.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(key), Some(val)) = (parts.next(), parts.next()) {
            let num: u64 = val.parse().unwrap_or(0);
            match key {
                "usage_usec" => usage_us = num,
                "user_usec" => user_us = num,
                "system_usec" => system_us = num,
                "nr_throttled" => nr_throttled = num,
                "throttled_usec" => throttled_us = num,
                _ => {}
            }
        }
    }

    (usage_us, user_us, system_us, nr_throttled, throttled_us)
}

/// Parses a single integer byte counter (e.g. from `memory.current` or `memory.peak`).
pub fn parse_memory_bytes(content: &str) -> u64 {
    content.trim().parse().unwrap_or(0)
}

/// Reads cgroup v2 metrics from the session's cgroup slice directory.
/// If the directory or files are not accessible (e.g. on non-Linux hosts or unmounted cgroups),
/// falls back to returning a default/empty structure without failing closed.
pub fn read_cgroup_v2_stats(cgroup_dir: &Path) -> Result<CgroupResourceUsage, VamTelemetryError> {
    if !cgroup_dir.exists() {
        return Ok(CgroupResourceUsage::default());
    }

    let mut usage = CgroupResourceUsage::default();

    // 1. Read cpu.stat
    let cpu_stat_path = cgroup_dir.join("cpu.stat");
    if let Ok(content) = fs::read_to_string(&cpu_stat_path) {
        let (usage_us, user_us, sys_us, nr_throt, throt_us) = parse_cpu_stat(&content);
        usage.cpu_usage_us = usage_us;
        usage.cpu_user_us = user_us;
        usage.cpu_system_us = sys_us;
        usage.nr_throttled = nr_throt;
        usage.cpu_throttled_us = throt_us;
    }

    // 2. Read memory.current
    let mem_cur_path = cgroup_dir.join("memory.current");
    if let Ok(content) = fs::read_to_string(&mem_cur_path) {
        usage.memory_current_bytes = parse_memory_bytes(&content);
    }

    // 3. Read memory.peak
    let mem_peak_path = cgroup_dir.join("memory.peak");
    if let Ok(content) = fs::read_to_string(&mem_peak_path) {
        usage.memory_peak_bytes = parse_memory_bytes(&content);
    }

    Ok(usage)
}

/// Records cgroup v2 metrics into the metrics registry and emits session-level tracing events.
pub fn record_cgroup_metrics(
    session_id: &str,
    usage: &CgroupResourceUsage,
    registry: &VamMetricsRegistry,
) -> Result<(), VamTelemetryError> {
    if usage.cpu_usage_us > 0 {
        registry.set_counter(
            METRIC_CGROUP_CPU_USAGE_US,
            usage.cpu_usage_us,
            &[("subsystem", "cgroup")],
        )?;
    }

    if usage.cpu_throttled_us > 0 {
        registry.set_counter(
            METRIC_CGROUP_THROTTLED_US,
            usage.cpu_throttled_us,
            &[("subsystem", "cgroup")],
        )?;
    }

    if usage.memory_current_bytes > 0 {
        registry.set_gauge(
            METRIC_CGROUP_MEMORY_CURRENT_BYTES,
            usage.memory_current_bytes as i64,
            &[("subsystem", "cgroup")],
        )?;
    }

    if usage.memory_peak_bytes > 0 {
        registry.set_gauge(
            METRIC_CGROUP_MEMORY_PEAK_BYTES,
            usage.memory_peak_bytes as i64,
            &[("subsystem", "cgroup")],
        )?;
    }

    info!(
        target: "vam_cgroup",
        session_id = %session_id,
        cpu_usage_ms = usage.cpu_usage_ms(),
        cpu_throttled_ms = usage.throttled_ms(),
        memory_current_bytes = usage.memory_current_bytes,
        memory_peak_mb = usage.memory_peak_mb(),
        "Recorded host cgroup v2 resource attribution"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cpu_stat_and_memory() {
        let cpu_stat = r#"usage_usec 2500000
user_usec 1800000
system_usec 700000
nr_periods 50
nr_throttled 3
throttled_usec 45000"#;

        let (usage_us, user_us, sys_us, nr_throt, throt_us) = parse_cpu_stat(cpu_stat);
        assert_eq!(usage_us, 2_500_000);
        assert_eq!(user_us, 1_800_000);
        assert_eq!(sys_us, 700_000);
        assert_eq!(nr_throt, 3);
        assert_eq!(throt_us, 45_000);

        let mem_current = "134217728\n";
        assert_eq!(parse_memory_bytes(mem_current), 134_217_728);
    }

    #[test]
    fn test_read_cgroup_v2_stats_from_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = temp_dir.path();

        fs::write(
            dir.join("cpu.stat"),
            "usage_usec 5000000\nuser_usec 3000000\nsystem_usec 2000000\nnr_throttled 1\nthrottled_usec 10000\n",
        )
        .unwrap();
        fs::write(dir.join("memory.current"), "67108864\n").unwrap();
        fs::write(dir.join("memory.peak"), "104857600\n").unwrap();

        let usage = read_cgroup_v2_stats(dir).unwrap();
        assert_eq!(usage.cpu_usage_ms(), 5000);
        assert_eq!(usage.throttled_ms(), 10);
        assert_eq!(usage.memory_current_bytes, 67_108_864);
        assert_eq!(usage.memory_peak_bytes, 104_857_600);

        let registry = VamMetricsRegistry::new();
        record_cgroup_metrics("session-cg-1", &usage, &registry).unwrap();

        assert_eq!(
            registry.get_counter(METRIC_CGROUP_CPU_USAGE_US, &[("subsystem", "cgroup")]),
            Some(5_000_000)
        );
        assert_eq!(
            registry.get_gauge(METRIC_CGROUP_MEMORY_PEAK_BYTES, &[("subsystem", "cgroup")]),
            Some(104_857_600)
        );
    }
}
