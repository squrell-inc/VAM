//! Multi-tenancy isolation models, tenant-scoped spools, and mTLS/SPIFFE validation (Phase 7.3).

use crate::error::VamTelemetryError;
use crate::queue::DiskSpoolQueue;
use crate::retention::RetentionPolicy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Single-node tenant env (`VAMD_TENANT_ID`). Default `local`. Not a control-plane IAM id.
pub const ENV_TENANT_ID: &str = "VAMD_TENANT_ID";
pub const DEFAULT_TENANT_ID: &str = "local";
pub const ENV_OTLP_TLS_CA: &str = "VAMD_OTLP_TLS_CA";
pub const ENV_OTLP_TLS_CERT: &str = "VAMD_OTLP_TLS_CERT";
pub const ENV_OTLP_TLS_KEY: &str = "VAMD_OTLP_TLS_KEY";
pub const ENV_OTLP_SPIFFE_ID: &str = "VAMD_OTLP_SPIFFE_ID";

static INSTALLED_TENANT: OnceLock<TenantContext> = OnceLock::new();

/// Validated Tenant Identifier.
/// Enforces alphanumeric, dash, and underscore constraints to prevent directory traversal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantId(String);

impl TenantId {
    pub fn new(id: impl Into<String>) -> Result<Self, VamTelemetryError> {
        let raw = id.into();
        if raw.is_empty() || raw.len() > 64 {
            return Err(VamTelemetryError::TenantIsolationViolation {
                tenant_id: raw,
                reason: "tenant ID length must be between 1 and 64 characters".to_string(),
            });
        }

        // Must be safe for filesystem pathing and partitioning (no slashes, colons, or traversal dots)
        if !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(VamTelemetryError::TenantIsolationViolation {
                tenant_id: raw,
                reason: "tenant ID must contain only alphanumeric ASCII characters, dashes, or underscores".to_string(),
            });
        }

        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Default single-node tenant (`local`).
    pub fn local() -> Self {
        Self(DEFAULT_TENANT_ID.to_string())
    }

    /// `VAMD_TENANT_ID` or `local`. Invalid values (including `../`) are rejected.
    pub fn from_env() -> Result<Self, VamTelemetryError> {
        match std::env::var(ENV_TENANT_ID) {
            Ok(v) if !v.trim().is_empty() => Self::new(v.trim()),
            _ => Ok(Self::local()),
        }
    }
}

/// Tenant context encapsulating data isolation boundaries.
#[derive(Debug, Clone)]
pub struct TenantContext {
    pub tenant_id: TenantId,
    pub retention_policy: RetentionPolicy,
    pub spool_root: PathBuf,
}

impl TenantContext {
    pub fn new(tenant_id: TenantId, spool_root: impl AsRef<Path>) -> Self {
        Self {
            tenant_id,
            retention_policy: RetentionPolicy::default(),
            spool_root: spool_root.as_ref().to_path_buf(),
        }
    }

    /// Generates a tenant-isolated partition key for the cryptographic audit ledger.
    pub fn partition_key(tenant_id: &str, logical_name: &str) -> String {
        format!("tenant:{}:{}", tenant_id, logical_name)
    }

    /// Generates a tenant-isolated partition key for the cryptographic audit ledger.
    pub fn audit_partition(&self, logical_name: &str) -> String {
        Self::partition_key(self.tenant_id.as_str(), logical_name)
    }

    /// Resolves the dedicated, isolated spool directory for this tenant.
    pub fn tenant_spool_dir(&self) -> PathBuf {
        self.spool_root.join(self.tenant_id.as_str())
    }

    /// Audit JSONL directory: `{spool}/{tenant}/audit/`.
    pub fn tenant_audit_dir(&self) -> PathBuf {
        self.tenant_spool_dir().join("audit")
    }
}

/// Install the process-wide tenant context (once). Later calls are ignored.
pub fn install_tenant_context(ctx: TenantContext) {
    let _ = INSTALLED_TENANT.set(ctx);
}

/// Installed tenant, or `VAMD_TENANT_ID` / `local` under `spool_root`.
pub fn current_tenant_id() -> TenantId {
    if let Some(ctx) = INSTALLED_TENANT.get() {
        return ctx.tenant_id.clone();
    }
    TenantId::from_env().unwrap_or_else(|_| TenantId::local())
}

/// Ledger partition `tenant:{id}:{session}`.
pub fn current_audit_partition(session_id: &str) -> String {
    TenantContext::partition_key(current_tenant_id().as_str(), session_id)
}

/// `{VAMD_SPOOL_DIR}/{tenant}` when the env is set (or the installed context).
pub fn resolve_tenant_spool_dir(spool_root: Option<&Path>) -> Option<PathBuf> {
    if let Some(ctx) = INSTALLED_TENANT.get() {
        return Some(ctx.tenant_spool_dir());
    }
    let root = spool_root.map(|p| p.to_path_buf()).or_else(|| {
        std::env::var("VAMD_SPOOL_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    })?;
    let tenant = current_tenant_id();
    Some(root.join(tenant.as_str()))
}

/// Thread-safe manager for per-tenant disk spool queues.
#[derive(Debug)]
pub struct TenantSpoolManager {
    base_dir: PathBuf,
    queues: Mutex<HashMap<TenantId, Arc<DiskSpoolQueue>>>,
}

impl TenantSpoolManager {
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
            queues: Mutex::new(HashMap::new()),
        }
    }

    /// Retrieves or initializes the isolated `DiskSpoolQueue` for a tenant.
    pub fn get_or_create_spool(
        &self,
        tenant_id: &TenantId,
        max_bytes: u64,
    ) -> Result<Arc<DiskSpoolQueue>, VamTelemetryError> {
        let mut queues = self
            .queues
            .lock()
            .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                path: self.base_dir.display().to_string(),
                details: format!("mutex poisoned: {e}"),
            })?;

        if let Some(queue) = queues.get(tenant_id) {
            return Ok(queue.clone());
        }

        let tenant_path = self.base_dir.join(tenant_id.as_str());
        std::fs::create_dir_all(&tenant_path).map_err(|e| VamTelemetryError::SpoolDiskFailure {
            path: tenant_path.display().to_string(),
            details: format!("failed to create tenant spool directory: {e}"),
        })?;

        let queue = Arc::new(DiskSpoolQueue::new(
            tenant_path.join("spool.log"),
            100,
            max_bytes,
        ));
        queues.insert(tenant_id.clone(), queue.clone());
        Ok(queue)
    }
}

/// Remote OTel collector mTLS and SPIFFE ID authentication configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CollectorTlsConfig {
    pub enabled: bool,
    pub ca_cert_path: Option<String>,
    pub client_cert_path: Option<String>,
    pub client_key_path: Option<String>,
    pub expected_spiffe_id: Option<String>,
}

impl CollectorTlsConfig {
    pub fn from_env() -> Self {
        let ca_cert_path = env_nonempty(ENV_OTLP_TLS_CA);
        let client_cert_path = env_nonempty(ENV_OTLP_TLS_CERT);
        let client_key_path = env_nonempty(ENV_OTLP_TLS_KEY);
        let expected_spiffe_id = env_nonempty(ENV_OTLP_SPIFFE_ID);
        let any_set = ca_cert_path.is_some()
            || client_cert_path.is_some()
            || client_key_path.is_some()
            || expected_spiffe_id.is_some();
        Self {
            enabled: any_set,
            ca_cert_path,
            client_cert_path,
            client_key_path,
            expected_spiffe_id,
        }
    }

    /// CA + client cert + key paths are all present (mTLS material).
    pub fn has_mtls_material(&self) -> bool {
        nonempty(self.ca_cert_path.as_deref())
            && nonempty(self.client_cert_path.as_deref())
            && nonempty(self.client_key_path.as_deref())
    }

    /// Validates an incoming or peer SPIFFE ID against the expected trust pattern.
    /// No SPIFFE SDK: this is a string compare of the configured expected ID.
    pub fn validate_spiffe_id(&self, peer_spiffe_id: &str) -> bool {
        if !self.enabled {
            return true;
        }
        match &self.expected_spiffe_id {
            Some(expected) => peer_spiffe_id == expected,
            None => true,
        }
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn nonempty(val: Option<&str>) -> bool {
    val.is_some_and(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_tenant_id_validation() {
        assert!(TenantId::new("tenant-acme-1").is_ok());
        assert!(TenantId::new("corp_finance_prod").is_ok());

        // Directory traversal & invalid character attempts must fail
        assert!(TenantId::new("../malicious").is_err());
        assert!(TenantId::new("tenant/subtenant").is_err());
        assert!(TenantId::new("").is_err());
        assert!(TenantId::new("tenant:acme").is_err());
    }

    #[test]
    fn test_tenant_context_partitioning() {
        let tenant = TenantId::new("acme-corp").unwrap();
        let ctx = TenantContext::new(tenant, "/var/log/vam/spool");

        assert_eq!(
            ctx.audit_partition("vam_audit_evaluation"),
            "tenant:acme-corp:vam_audit_evaluation"
        );
        assert!(ctx.tenant_spool_dir().ends_with("acme-corp"));
    }

    #[test]
    fn test_tenant_spool_manager_isolation() {
        let temp = tempdir().expect("tempdir");
        let manager = TenantSpoolManager::new(temp.path());

        let tenant_a = TenantId::new("tenant-alpha").unwrap();
        let tenant_b = TenantId::new("tenant-beta").unwrap();

        let queue_a = manager.get_or_create_spool(&tenant_a, 1024 * 1024).unwrap();
        let queue_b = manager.get_or_create_spool(&tenant_b, 1024 * 1024).unwrap();

        // Queues must point to isolated directory paths
        assert!(temp.path().join("tenant-alpha").exists());
        assert!(temp.path().join("tenant-beta").exists());
        queue_a.enqueue(b"event_a".to_vec()).unwrap();
        assert_eq!(queue_a.memory_len(), 1);
        assert_eq!(queue_b.memory_len(), 0);
    }

    #[test]
    fn test_collector_spiffe_validation() {
        let mut config = CollectorTlsConfig {
            enabled: true,
            ca_cert_path: Some("/etc/ssl/ca.pem".to_string()),
            client_cert_path: Some("/etc/ssl/cert.pem".to_string()),
            client_key_path: Some("/etc/ssl/key.pem".to_string()),
            expected_spiffe_id: Some("spiffe://acme.internal/ns/vam/sa/collector".to_string()),
        };

        assert!(config.validate_spiffe_id("spiffe://acme.internal/ns/vam/sa/collector"));
        assert!(!config.validate_spiffe_id("spiffe://evil.com/attacker"));

        // Disabled config permits all
        config.enabled = false;
        assert!(config.validate_spiffe_id("any-id"));
    }

    #[test]
    fn two_tenants_write_isolated_dirs() {
        let temp = tempdir().expect("tempdir");
        let a = TenantId::new("acme").unwrap();
        let b = TenantId::new("globex").unwrap();
        let ctx_a = TenantContext::new(a, temp.path());
        let ctx_b = TenantContext::new(b, temp.path());
        std::fs::create_dir_all(ctx_a.tenant_spool_dir()).unwrap();
        std::fs::create_dir_all(ctx_b.tenant_spool_dir()).unwrap();
        std::fs::write(ctx_a.tenant_spool_dir().join("cloudevents.jsonl"), b"a\n").unwrap();
        std::fs::write(ctx_b.tenant_spool_dir().join("cloudevents.jsonl"), b"b\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(ctx_a.tenant_spool_dir().join("cloudevents.jsonl")).unwrap(),
            "a\n"
        );
        assert_eq!(
            std::fs::read_to_string(ctx_b.tenant_spool_dir().join("cloudevents.jsonl")).unwrap(),
            "b\n"
        );
        assert_ne!(ctx_a.tenant_spool_dir(), ctx_b.tenant_spool_dir());
        assert!(TenantId::new("../evil").is_err());
    }
}
