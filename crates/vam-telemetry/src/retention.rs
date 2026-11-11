use crate::error::VamTelemetryError;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Optional per-class spool caps. Unset keeps [`RetentionPolicy`] defaults.
pub const ENV_SPOOL_MAX_A_BYTES: &str = "VAMD_SPOOL_MAX_A_BYTES";
pub const ENV_SPOOL_MAX_B_BYTES: &str = "VAMD_SPOOL_MAX_B_BYTES";
pub const ENV_SPOOL_MAX_C_BYTES: &str = "VAMD_SPOOL_MAX_C_BYTES";
pub const ENV_SPOOL_MAX_D_BYTES: &str = "VAMD_SPOOL_MAX_D_BYTES";

const DEFAULT_CLASS_B_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const DEFAULT_CLASS_C_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const DEFAULT_CLASS_D_AGE: Duration = Duration::from_secs(90 * 24 * 60 * 60);

static GLOBAL_RETENTION: OnceLock<Arc<RetentionManager>> = OnceLock::new();

/// Signal-specific retention classes (Decision T24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(non_camel_case_types)]
pub enum RetentionClass {
    /// Class A: Cryptographic audit ledger, signed approvals (1-7 years, high durability).
    ClassA_Audit,
    /// Class B: Operational CloudEvents, workflow state transitions (14-30 days).
    ClassB_Operational,
    /// Class C: Diagnostic OTLP traces, spans, lossy ring buffer (3-7 days, loss-tolerant).
    ClassC_Diagnostic,
    /// Class D: Aggregated system metrics (90 days).
    ClassD_SystemMetrics,
}

impl RetentionClass {
    pub fn name(&self) -> &'static str {
        match self {
            RetentionClass::ClassA_Audit => "ClassA_Audit",
            RetentionClass::ClassB_Operational => "ClassB_Operational",
            RetentionClass::ClassC_Diagnostic => "ClassC_Diagnostic",
            RetentionClass::ClassD_SystemMetrics => "ClassD_SystemMetrics",
        }
    }

    /// Eviction priority under disk pressure (lower number = evicted first).
    pub fn eviction_priority(&self) -> u8 {
        match self {
            RetentionClass::ClassC_Diagnostic => 1,
            RetentionClass::ClassB_Operational => 2,
            RetentionClass::ClassD_SystemMetrics => 3,
            RetentionClass::ClassA_Audit => 255, // Never silently evicted
        }
    }
}

/// Spool quota policy configuring maximum disk boundaries per retention class (Decision T25).
#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    pub max_class_a_bytes: u64,
    pub max_class_b_bytes: u64,
    pub max_class_c_bytes: u64,
    pub max_class_d_bytes: u64,
    /// Class B age cap. Class A is never age-deleted.
    pub class_b_max_age: Duration,
    pub class_c_max_age: Duration,
    pub class_d_max_age: Duration,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_class_a_bytes: 5 * 1024 * 1024 * 1024, // 5 GB audit
            max_class_b_bytes: 1024 * 1024 * 1024,     // 1 GB operational
            max_class_c_bytes: 512 * 1024 * 1024,      // 512 MB diagnostic traces
            max_class_d_bytes: 256 * 1024 * 1024,      // 256 MB system metrics
            class_b_max_age: DEFAULT_CLASS_B_AGE,
            class_c_max_age: DEFAULT_CLASS_C_AGE,
            class_d_max_age: DEFAULT_CLASS_D_AGE,
        }
    }
}

impl RetentionPolicy {
    pub fn from_env() -> Self {
        let mut policy = Self::default();
        if let Some(v) = parse_u64_env(ENV_SPOOL_MAX_A_BYTES) {
            policy.max_class_a_bytes = v;
        }
        if let Some(v) = parse_u64_env(ENV_SPOOL_MAX_B_BYTES) {
            policy.max_class_b_bytes = v;
        }
        if let Some(v) = parse_u64_env(ENV_SPOOL_MAX_C_BYTES) {
            policy.max_class_c_bytes = v;
        }
        if let Some(v) = parse_u64_env(ENV_SPOOL_MAX_D_BYTES) {
            policy.max_class_d_bytes = v;
        }
        policy
    }

    pub fn max_age(&self, class: RetentionClass) -> Option<Duration> {
        match class {
            RetentionClass::ClassA_Audit => None,
            RetentionClass::ClassB_Operational => Some(self.class_b_max_age),
            RetentionClass::ClassC_Diagnostic => Some(self.class_c_max_age),
            RetentionClass::ClassD_SystemMetrics => Some(self.class_d_max_age),
        }
    }
}

fn parse_u64_env(key: &str) -> Option<u64> {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

/// Thread-safe retention and spool quota manager.
/// Protects host disk space and ensures Class A audit records are preserved over lossy traces.
#[derive(Debug)]
pub struct RetentionManager {
    policy: RetentionPolicy,
    usage: Mutex<HashMap<RetentionClass, u64>>,
}

impl RetentionManager {
    pub fn new(policy: RetentionPolicy) -> Self {
        Self {
            policy,
            usage: Mutex::new(HashMap::new()),
        }
    }

    /// Check if recording `bytes` for `class` stays within quota limits.
    /// If within limits, updates usage. If Class A exceeds limit, returns fail-closed StorageQuotaExceeded.
    pub fn record_usage(&self, class: RetentionClass, bytes: u64) -> Result<(), VamTelemetryError> {
        let mut usage = self
            .usage
            .lock()
            .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                path: "retention_manager".to_string(),
                details: format!("mutex poisoned: {e}"),
            })?;

        let current = usage.entry(class).or_insert(0);
        let limit = self.limit_for_class(class);

        if current.saturating_add(bytes) > limit {
            return Err(VamTelemetryError::StorageQuotaExceeded {
                class: class.name().to_string(),
                usage_bytes: *current + bytes,
                limit_bytes: limit,
            });
        }

        *current = current.saturating_add(bytes);
        Ok(())
    }

    /// Prunes bytes from lossy lanes (Class C diagnostic traces first, then Class B operational events).
    /// Class A audit records are strictly protected and never pruned.
    pub fn prune_lossy(&self, target_bytes: u64) -> u64 {
        let mut usage = match self.usage.lock() {
            Ok(u) => u,
            Err(_) => return 0,
        };

        let mut freed = 0u64;

        // 1. Prune Class C first
        if let Some(c_usage) = usage.get_mut(&RetentionClass::ClassC_Diagnostic) {
            let to_free = (*c_usage).min(target_bytes - freed);
            *c_usage = c_usage.saturating_sub(to_free);
            freed += to_free;
        }

        // 2. Prune Class B next if needed
        if freed < target_bytes {
            if let Some(b_usage) = usage.get_mut(&RetentionClass::ClassB_Operational) {
                let to_free = (*b_usage).min(target_bytes - freed);
                *b_usage = b_usage.saturating_sub(to_free);
                freed += to_free;
            }
        }

        freed
    }

    pub fn current_usage(&self, class: RetentionClass) -> u64 {
        self.usage
            .lock()
            .ok()
            .and_then(|u| u.get(&class).copied())
            .unwrap_or(0)
    }

    pub fn limit_for_class(&self, class: RetentionClass) -> u64 {
        match class {
            RetentionClass::ClassA_Audit => self.policy.max_class_a_bytes,
            RetentionClass::ClassB_Operational => self.policy.max_class_b_bytes,
            RetentionClass::ClassC_Diagnostic => self.policy.max_class_c_bytes,
            RetentionClass::ClassD_SystemMetrics => self.policy.max_class_d_bytes,
        }
    }

    pub fn policy(&self) -> &RetentionPolicy {
        &self.policy
    }

    /// Replace measured usage (sweeper syncs from disk).
    pub fn set_usage(&self, class: RetentionClass, bytes: u64) -> Result<(), VamTelemetryError> {
        let mut usage = self
            .usage
            .lock()
            .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                path: "retention_manager".to_string(),
                details: format!("mutex poisoned: {e}"),
            })?;
        usage.insert(class, bytes);
        Ok(())
    }

    /// Fail-closed check without mutating usage (Class A persist).
    pub fn check_quota(
        &self,
        class: RetentionClass,
        additional: u64,
    ) -> Result<(), VamTelemetryError> {
        let current = self.current_usage(class);
        let limit = self.limit_for_class(class);
        if current.saturating_add(additional) > limit {
            return Err(VamTelemetryError::StorageQuotaExceeded {
                class: class.name().to_string(),
                usage_bytes: current.saturating_add(additional),
                limit_bytes: limit,
            });
        }
        Ok(())
    }
}

/// Install the process-wide retention manager (once).
pub fn install_retention_manager(manager: Arc<RetentionManager>) {
    let _ = GLOBAL_RETENTION.set(manager);
}

pub fn retention_manager() -> Option<Arc<RetentionManager>> {
    GLOBAL_RETENTION.get().cloned()
}

/// Result of a real-file sweep under a tenant spool directory.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub freed_bytes: u64,
    pub class_a_bytes: u64,
    pub class_b_bytes: u64,
    pub class_c_bytes: u64,
    pub class_d_bytes: u64,
    pub files_pruned: u64,
    pub class_a_over_quota: bool,
}

#[derive(Debug, Clone)]
struct SpoolFile {
    path: PathBuf,
    class: RetentionClass,
    size: u64,
    age: Duration,
}

/// Classify a path relative to the tenant spool root.
/// `audit/` = A; CloudEvents JSONL / HTTP spool = B; diagnostic/OTLP leftovers = C.
pub fn classify_spool_relpath(rel: &Path) -> Option<RetentionClass> {
    let mut comps = rel.components();
    let first = comps
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned());
    let first = first.as_deref()?;
    if first == "audit" {
        return Some(RetentionClass::ClassA_Audit);
    }
    if matches!(first, "diagnostic" | "otlp" | "traces") {
        return Some(RetentionClass::ClassC_Diagnostic);
    }
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.contains("diagnostic") || name.ends_with(".otlp") || name.ends_with(".pb") {
        return Some(RetentionClass::ClassC_Diagnostic);
    }
    if name.ends_with(".jsonl") || name.ends_with(".spool") || name.ends_with(".cursor") {
        return Some(RetentionClass::ClassB_Operational);
    }
    None
}

/// Walk `{tenant_spool}`, prune Class C then B (age + quota). Never deletes `audit/`.
pub fn sweep_tenant_spool(tenant_spool: &Path, manager: &RetentionManager) -> SweepReport {
    let mut report = SweepReport::default();
    let mut files = match collect_spool_files(tenant_spool) {
        Ok(f) => f,
        Err(_) => return report,
    };

    let mut measured = measure_classes(&files);
    report.class_a_bytes = measured.a;
    report.class_b_bytes = measured.b;
    report.class_c_bytes = measured.c;
    report.class_d_bytes = measured.d;

    // Age prune C then B (and D). Never age-delete Class A.
    for class in [
        RetentionClass::ClassC_Diagnostic,
        RetentionClass::ClassB_Operational,
        RetentionClass::ClassD_SystemMetrics,
    ] {
        if let Some(max_age) = manager.policy().max_age(class) {
            let (freed, pruned) = prune_aged(&mut files, class, max_age);
            report.freed_bytes = report.freed_bytes.saturating_add(freed);
            report.files_pruned = report.files_pruned.saturating_add(pruned);
        }
    }

    measured = measure_classes(&files);

    // Quota prune C then B. Never delete audit.
    for class in [
        RetentionClass::ClassC_Diagnostic,
        RetentionClass::ClassB_Operational,
        RetentionClass::ClassD_SystemMetrics,
    ] {
        let usage = class_bytes(&measured, class);
        let limit = manager.limit_for_class(class);
        if usage > limit {
            let need = usage - limit;
            let (freed, pruned) = prune_oldest(&mut files, class, need);
            report.freed_bytes = report.freed_bytes.saturating_add(freed);
            report.files_pruned = report.files_pruned.saturating_add(pruned);
        }
    }

    measured = measure_classes(&files);
    report.class_a_bytes = measured.a;
    report.class_b_bytes = measured.b;
    report.class_c_bytes = measured.c;
    report.class_d_bytes = measured.d;
    report.class_a_over_quota = measured.a > manager.limit_for_class(RetentionClass::ClassA_Audit);

    let _ = manager.set_usage(RetentionClass::ClassA_Audit, measured.a);
    let _ = manager.set_usage(RetentionClass::ClassB_Operational, measured.b);
    let _ = manager.set_usage(RetentionClass::ClassC_Diagnostic, measured.c);
    let _ = manager.set_usage(RetentionClass::ClassD_SystemMetrics, measured.d);

    report
}

#[derive(Default, Clone, Copy)]
struct ClassSizes {
    a: u64,
    b: u64,
    c: u64,
    d: u64,
}

fn class_bytes(sizes: &ClassSizes, class: RetentionClass) -> u64 {
    match class {
        RetentionClass::ClassA_Audit => sizes.a,
        RetentionClass::ClassB_Operational => sizes.b,
        RetentionClass::ClassC_Diagnostic => sizes.c,
        RetentionClass::ClassD_SystemMetrics => sizes.d,
    }
}

fn measure_classes(files: &[SpoolFile]) -> ClassSizes {
    let mut sizes = ClassSizes::default();
    for f in files {
        match f.class {
            RetentionClass::ClassA_Audit => sizes.a = sizes.a.saturating_add(f.size),
            RetentionClass::ClassB_Operational => sizes.b = sizes.b.saturating_add(f.size),
            RetentionClass::ClassC_Diagnostic => sizes.c = sizes.c.saturating_add(f.size),
            RetentionClass::ClassD_SystemMetrics => sizes.d = sizes.d.saturating_add(f.size),
        }
    }
    sizes
}

fn collect_spool_files(root: &Path) -> Result<Vec<SpoolFile>, std::io::Error> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    walk_collect(root, root, &mut out)?;
    Ok(out)
}

fn walk_collect(root: &Path, dir: &Path, out: &mut Vec<SpoolFile>) -> Result<(), std::io::Error> {
    let entries = fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        if path.is_dir() {
            walk_collect(root, &path, out)?;
            continue;
        }
        let Some(class) = classify_spool_relpath(rel) else {
            continue;
        };
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let age = meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .unwrap_or_default();
        out.push(SpoolFile {
            path,
            class,
            size: meta.len(),
            age,
        });
    }
    Ok(())
}

fn prune_aged(files: &mut Vec<SpoolFile>, class: RetentionClass, max_age: Duration) -> (u64, u64) {
    let mut freed = 0u64;
    let mut pruned = 0u64;
    let mut keep = Vec::with_capacity(files.len());
    for file in files.drain(..) {
        if file.class == class
            && file.age > max_age
            && class != RetentionClass::ClassA_Audit
            && delete_or_truncate(&file, file.size)
        {
            freed = freed.saturating_add(file.size);
            pruned = pruned.saturating_add(1);
            continue;
        }
        keep.push(file);
    }
    *files = keep;
    (freed, pruned)
}

fn prune_oldest(files: &mut Vec<SpoolFile>, class: RetentionClass, need: u64) -> (u64, u64) {
    if class == RetentionClass::ClassA_Audit || need == 0 {
        return (0, 0);
    }
    let mut candidates: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.class == class)
        .map(|(i, _)| i)
        .collect();
    candidates.sort_by_key(|&i| files[i].age);
    candidates.reverse(); // oldest first

    let mut freed = 0u64;
    let mut pruned = 0u64;
    let mut remove_idx = Vec::new();
    for idx in candidates {
        if freed >= need {
            break;
        }
        let file = &files[idx];
        let want = need.saturating_sub(freed);
        if is_jsonl(&file.path) && file.size > want {
            let dropped = truncate_jsonl_from_front(&file.path, want);
            if dropped > 0 {
                freed = freed.saturating_add(dropped);
                pruned = pruned.saturating_add(1);
                files[idx].size = files[idx].size.saturating_sub(dropped);
            }
            continue;
        }
        if delete_or_truncate(file, file.size) {
            freed = freed.saturating_add(file.size);
            pruned = pruned.saturating_add(1);
            remove_idx.push(idx);
        }
    }
    remove_idx.sort_unstable();
    remove_idx.reverse();
    for idx in remove_idx {
        files.remove(idx);
    }
    (freed, pruned)
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonl")
}

fn delete_or_truncate(file: &SpoolFile, _bytes: u64) -> bool {
    if file.class == RetentionClass::ClassA_Audit {
        return false;
    }
    fs::remove_file(&file.path).is_ok()
}

fn truncate_jsonl_from_front(path: &Path, target_free: u64) -> u64 {
    let Ok(data) = fs::read_to_string(path) else {
        return 0;
    };
    let mut dropped = 0u64;
    let mut start = 0usize;
    for line in data.split_inclusive('\n') {
        if dropped >= target_free {
            break;
        }
        dropped = dropped.saturating_add(line.len() as u64);
        start = start.saturating_add(line.len());
    }
    let rest = if start >= data.len() {
        ""
    } else {
        &data[start..]
    };
    if fs::write(path, rest).is_ok() {
        dropped
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_retention_quota_enforcement_and_pruning() {
        let policy = RetentionPolicy {
            max_class_a_bytes: 1000,
            max_class_b_bytes: 1000,
            max_class_c_bytes: 1000,
            max_class_d_bytes: 1000,
            ..RetentionPolicy::default()
        };
        let manager = RetentionManager::new(policy);

        // Record usage
        assert!(manager
            .record_usage(RetentionClass::ClassC_Diagnostic, 500)
            .is_ok());
        assert!(manager
            .record_usage(RetentionClass::ClassB_Operational, 400)
            .is_ok());
        assert!(manager
            .record_usage(RetentionClass::ClassA_Audit, 300)
            .is_ok());

        assert_eq!(
            manager.current_usage(RetentionClass::ClassC_Diagnostic),
            500
        );

        // Exceeding Class A quota fails closed
        assert!(manager
            .record_usage(RetentionClass::ClassA_Audit, 800)
            .is_err());

        // Pruning drops Class C first
        let freed = manager.prune_lossy(400);
        assert_eq!(freed, 400);
        assert_eq!(
            manager.current_usage(RetentionClass::ClassC_Diagnostic),
            100
        );
        // Class A remains untouched
        assert_eq!(manager.current_usage(RetentionClass::ClassA_Audit), 300);
    }

    #[test]
    fn sweeper_prunes_c_then_b_keeps_audit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir_all(root.join("audit")).unwrap();
        fs::create_dir_all(root.join("diagnostic")).unwrap();

        let audit_body = "audit-keep-this-record\n".repeat(4);
        fs::write(root.join("audit").join("sess.jsonl"), &audit_body).unwrap();

        let c_line = "c-diagnostic-line-xxxxxxxx\n";
        let mut c_body = String::new();
        for _ in 0..20 {
            c_body.push_str(c_line);
        }
        fs::write(root.join("diagnostic").join("cache.jsonl"), &c_body).unwrap();

        let b_line = "b-operational-line-xxxxxxxx\n";
        let mut b_body = String::new();
        for _ in 0..20 {
            b_body.push_str(b_line);
        }
        fs::write(root.join("cloudevents.jsonl"), &b_body).unwrap();

        let c_before = fs::metadata(root.join("diagnostic").join("cache.jsonl"))
            .unwrap()
            .len();
        let b_before = fs::metadata(root.join("cloudevents.jsonl")).unwrap().len();
        let a_before = fs::metadata(root.join("audit").join("sess.jsonl"))
            .unwrap()
            .len();
        assert!(c_before > 80);
        assert!(b_before > 80);

        let policy = RetentionPolicy {
            max_class_a_bytes: 10_000,
            max_class_b_bytes: 80,
            max_class_c_bytes: 80,
            max_class_d_bytes: 10_000,
            ..RetentionPolicy::default()
        };
        let manager = RetentionManager::new(policy);
        let report = sweep_tenant_spool(root, &manager);

        assert!(
            root.join("audit").join("sess.jsonl").exists(),
            "audit file must remain"
        );
        let a_after = fs::metadata(root.join("audit").join("sess.jsonl"))
            .unwrap()
            .len();
        assert_eq!(a_after, a_before, "Class A must not be pruned");
        assert!(!report.class_a_over_quota);
        assert!(report.freed_bytes > 0);

        let c_after = fs::metadata(root.join("diagnostic").join("cache.jsonl"))
            .map(|m| m.len())
            .unwrap_or(0);
        let b_after = fs::metadata(root.join("cloudevents.jsonl"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(c_after < c_before, "Class C must shrink");
        assert!(b_after < b_before, "Class B must shrink");
        assert!(c_after <= 80 || !root.join("diagnostic").join("cache.jsonl").exists());
        assert!(b_after <= 80 || !root.join("cloudevents.jsonl").exists());
    }
}
