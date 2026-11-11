//! Session-scoped Firecracker JSON + best-effort cgroup v2 scrape into `VamMetricsRegistry`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::task::AbortHandle;
use tracing::debug;
use vam_telemetry::{
    parse_firecracker_metrics, read_cgroup_v2_stats, record_cgroup_metrics,
    record_firecracker_metrics, VamMetricsRegistry,
};

fn scrape_tasks() -> &'static Mutex<HashMap<String, AbortHandle>> {
    static SCRAPE_TASKS: OnceLock<Mutex<HashMap<String, AbortHandle>>> = OnceLock::new();
    SCRAPE_TASKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Host path where `vamd` asks Firecracker to write metrics JSON lines.
pub fn firecracker_metrics_path(vm_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("vam-fc-metrics-{}.json", vm_id))
}

/// Best-effort jailer / cgroup v2 directory. Missing on macOS/Lima is OK.
pub fn resolve_cgroup_dir(vm_id: &str) -> Option<PathBuf> {
    let uid = std::env::var("VAMD_JAILER_UID").unwrap_or_else(|_| "0".to_string());
    let candidates = [
        PathBuf::from(format!("/sys/fs/cgroup/firecracker/{uid}/{vm_id}")),
        PathBuf::from(format!("/sys/fs/cgroup/firecracker/{vm_id}")),
        PathBuf::from(format!("/sys/fs/cgroup/{vm_id}")),
    ];
    candidates.into_iter().find(|p| p.is_dir())
}

/// Read the latest non-empty JSON line from a Firecracker metrics file. Skip if missing.
pub fn read_latest_metrics_line(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|s| s.to_string())
}

/// One scrape tick. Missing files/dirs are skipped; never fails the session.
pub fn scrape_once(
    session_id: &str,
    metrics_path: &Path,
    cgroup_dir: Option<&Path>,
    registry: &VamMetricsRegistry,
) {
    if let Some(line) = read_latest_metrics_line(metrics_path) {
        match parse_firecracker_metrics(&line) {
            Ok(snapshot) => {
                if let Err(e) = record_firecracker_metrics(session_id, &snapshot, registry) {
                    debug!(
                        session_id = %session_id,
                        error = %e,
                        "Skipping Firecracker metrics record"
                    );
                }
            }
            Err(e) => {
                debug!(
                    session_id = %session_id,
                    error = %e,
                    "Skipping unreadable Firecracker metrics line"
                );
            }
        }
    }

    if let Some(dir) = cgroup_dir {
        match read_cgroup_v2_stats(dir) {
            Ok(usage) => {
                if let Err(e) = record_cgroup_metrics(session_id, &usage, registry) {
                    debug!(
                        session_id = %session_id,
                        error = %e,
                        "Skipping cgroup metrics record"
                    );
                }
            }
            Err(e) => {
                debug!(
                    session_id = %session_id,
                    error = %e,
                    "Skipping cgroup stats"
                );
            }
        }
    }
}

async fn scrape_loop(session_id: String, vm_id: String) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let metrics_path = firecracker_metrics_path(&vm_id);
    let registry = VamMetricsRegistry::global();
    loop {
        interval.tick().await;
        let cgroup = resolve_cgroup_dir(&vm_id);
        scrape_once(
            &session_id,
            &metrics_path,
            cgroup.as_deref(),
            registry.as_ref(),
        );
    }
}

/// Start a ~1s scrape task for this session. Replaces any previous task for the same id.
pub fn spawn_session_metrics_scrape(session_id: String, vm_id: String) {
    stop_session_metrics_scrape(&session_id);
    let key = session_id.clone();
    let handle = tokio::spawn(async move {
        scrape_loop(session_id, vm_id).await;
    });
    let mut map = match scrape_tasks().lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    map.insert(key, handle.abort_handle());
}

/// Abort the scrape task on session teardown (same lifetime as brokers).
pub fn stop_session_metrics_scrape(session_id: &str) {
    let mut map = match scrape_tasks().lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    if let Some(handle) = map.remove(session_id) {
        handle.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_telemetry::{
        VamMetricsRegistry, METRIC_CGROUP_CPU_USAGE_US, METRIC_HYPERVISOR_VCPU_EXITS_TOTAL,
    };

    #[test]
    fn scrape_once_records_firecracker_and_cgroup() {
        let temp = tempfile::tempdir().unwrap();
        let metrics_path = temp.path().join("fc-metrics.json");
        std::fs::write(
            &metrics_path,
            r#"{"utc_timestamp_ms":1,"vcpu":{"exit_io_in":1,"exit_io_out":2,"exit_mmio_read":3,"exit_mmio_write":4},"block":{},"net":{},"vsock":{}}
"#,
        )
        .unwrap();

        let cgroup = temp.path().join("cgroup");
        std::fs::create_dir_all(&cgroup).unwrap();
        std::fs::write(
            cgroup.join("cpu.stat"),
            "usage_usec 2500000\nuser_usec 1\nsystem_usec 1\nnr_throttled 0\nthrottled_usec 0\n",
        )
        .unwrap();
        std::fs::write(cgroup.join("memory.current"), "1024\n").unwrap();

        let registry = VamMetricsRegistry::new();
        scrape_once("session-scrape-1", &metrics_path, Some(&cgroup), &registry);

        assert_eq!(
            registry.get_counter(METRIC_HYPERVISOR_VCPU_EXITS_TOTAL, &[("subsystem", "vcpu")]),
            Some(10)
        );
        assert_eq!(
            registry.get_counter(METRIC_CGROUP_CPU_USAGE_US, &[("subsystem", "cgroup")]),
            Some(2_500_000)
        );
    }

    #[test]
    fn scrape_once_skips_missing_paths() {
        let registry = VamMetricsRegistry::new();
        scrape_once(
            "session-scrape-missing",
            Path::new("/tmp/vam-fc-metrics-does-not-exist-phase0.json"),
            Some(Path::new("/tmp/vam-cgroup-does-not-exist-phase0")),
            &registry,
        );
        assert_eq!(
            registry.get_counter(METRIC_HYPERVISOR_VCPU_EXITS_TOTAL, &[("subsystem", "vcpu")]),
            None
        );
    }

    #[tokio::test]
    async fn scrape_task_stops_on_teardown() {
        spawn_session_metrics_scrape("session-abort".to_string(), "vm-abort".to_string());
        stop_session_metrics_scrape("session-abort");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let map = scrape_tasks().lock().unwrap_or_else(|e| e.into_inner());
        assert!(!map.contains_key("session-abort"));
    }
}
