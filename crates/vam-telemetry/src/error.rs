use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub enum VamTelemetryError {
    #[error("Validation failed: event schema mismatch for type '{event_type}'")]
    SchemaMismatch { event_type: String },

    #[error("Trace context validation failed: {details}")]
    InvalidTraceContext { details: String },

    #[error("Baggage limits exceeded: {details}")]
    BaggageExceeded { details: String },

    #[error("Invalid metric label: '{key}' is not permitted ({reason})")]
    InvalidLabel { key: String, reason: String },

    #[error("Buffer overflow: {lane} priority queue is fully saturated ({size} elements)")]
    QueueOverflow { lane: String, size: usize },

    #[error("Spool disk failure: failed to write to local storage path '{path}': {details}")]
    SpoolDiskFailure { path: String, details: String },

    #[error("OTLP exporter error: remote collector failed: {details}")]
    OtlpExportFailed { details: String },

    #[error("Audit ledger corrupted: hash mismatch at index {sequence}")]
    LedgerCorrupted { sequence: u64 },

    #[error("Audit signature verification failed at index {sequence}: {details}")]
    AuditSignatureInvalid { sequence: u64, details: String },

    #[error("Slow consumer evicted from telemetry stream: {reason}")]
    SlowConsumerEvicted { reason: String },

    #[error(
        "Storage quota exceeded for class '{class}': {usage_bytes}B used of {limit_bytes}B ceiling"
    )]
    StorageQuotaExceeded {
        class: String,
        usage_bytes: u64,
        limit_bytes: u64,
    },

    #[error("Hypervisor metrics parsing failed: {details}")]
    HypervisorMetricsParseFailed { details: String },

    #[error("Pricing catalog missing for billed model '{model_id}'")]
    MissingPricingCatalog { model_id: String },

    #[error("Attestation error: {reason}")]
    AttestationFailed { reason: String },

    #[error("Lineage error: {reason}")]
    LineageError { reason: String },

    #[error(
        "SLO target breached for tier '{tier}': observed {observed_ms}ms, target {target_ms}ms"
    )]
    SloBreached {
        tier: String,
        observed_ms: u64,
        target_ms: u64,
    },

    #[error("Tenant isolation violation for tenant '{tenant_id}': {reason}")]
    TenantIsolationViolation { tenant_id: String, reason: String },

    #[error("OCSF mapping failed: {reason}")]
    OcsfMappingFailed { reason: String },

    #[error("Evidence pack invalid: {reason}")]
    EvidencePackInvalid { reason: String },
}
