//! # vam-telemetry
//!
//! Host-centric telemetry, distributed tracing context propagation, metric conventions,
//! zero-I/O pre-spool redaction, and audit kernel for VAM.
//!
//! This crate enforces non-negotiable trust and stability invariants:
//! - Host-only OTLP export boundary (Decision T3)
//! - Untrusted W3C trace context validation & baggage limits (Decision T7)
//! - Metric low-cardinality enforcement (Decision T20)
//! - Pre-spool secret/PII redaction (Decision T21)

pub mod attestation;
pub mod audit;
pub mod bus;
pub mod capture;
pub mod cgroup;
pub mod dsse;
pub mod egress;
pub mod error;
pub mod evidence;
pub mod guardrail;
pub mod guest_ingress;
pub mod hypervisor;
pub mod lineage;
pub mod metrics;
pub mod ocsf;
pub mod otlp;
pub mod queue;
pub mod redact;
pub mod replay;
pub mod retention;
pub mod sampling;
pub mod semconv;
pub mod slo;
pub mod subscriber;
pub mod tenant;
pub mod trace_context;

pub use attestation::{
    CostSummary, EvaluationSummary, RunOutcomeAttestation, WorkspaceReceipt,
    RUN_OUTCOME_ATTESTATION_DOMAIN,
};
pub use audit::{verify_ledger_chain, AuditRecord, PartitionedAuditLedger, GENESIS_HASH};
pub use bus::{BroadcastEventBus, EventSubscriber, DEFAULT_BUS_CAPACITY, MAX_SLOW_CONSUMER_BYTES};
pub use capture::{TelemetryCapture, CAPTURE_PREVIEW_MAX_BYTES, ENV_TELEMETRY_CAPTURE};
pub use cgroup::{
    parse_cpu_stat, parse_memory_bytes, read_cgroup_v2_stats, record_cgroup_metrics,
    CgroupResourceUsage,
};
pub use dsse::{
    unwrap_attestation_dsse, wrap_attestation_dsse, DsseEnvelope, DsseSignature, DSSE_PAYLOAD_TYPE,
};
pub use egress::{
    append_jsonl, cloud_event_to_vec, dataschema_uri, focus_from_cost_summary, maybe_mirror_otlp,
    micro_to_decimal, parse_flag_default_false, project_cloud_event, project_outbox_event,
    project_vam_cloud_event, unix_ms_to_rfc3339, write_attestation_jsonl, write_cloudevents_jsonl,
    write_focus_jsonl, write_lineage_jsonl, write_ocsf_jsonl, CloudEventJson, FocusCostRecord,
    FocusTags, ATTESTATIONS_JSONL, CLOUDEVENTS_CONTENT_TYPE, CLOUDEVENTS_JSONL, DATASCHEMA_BASE,
    DEFAULT_SOURCE_URN, ENV_CLOUDEVENTS_MIRROR_OTLP, FOCUS_JSONL, LINEAGE_JSONL, OCSF_JSONL,
};
pub use error::VamTelemetryError;
pub use evidence::{
    build_manifest, finish_pack, is_deny_disposition, parse_jsonl, sha256_hex, to_jsonl,
    verify_evidence_pack, verify_evidence_pack_report, write_evidence_dir, ArtifactsPack,
    DenialRow, EvidenceManifest, EvidencePackResponse, PackReport, TrajectoryPack,
    TrajectoryPackEvent, FILE_ARTIFACTS, FILE_ATTESTATION, FILE_ATTESTATION_DSSE,
    FILE_ATTESTATION_ENVELOPE, FILE_AUDIT, FILE_DENIALS, FILE_MANIFEST, FILE_RECEIPT,
    FILE_TRAJECTORY, HASHED_FILES, OPTIONAL_HASHED_FILES,
};
pub use guardrail::{
    record_security_denied, record_tool_invoked_signal, GuardrailSignals, DENY_SPIKE_THRESHOLD,
    DENY_SPIKE_WINDOW, SAME_TOOL_LOOP_THRESHOLD,
};
pub use guest_ingress::{
    strip_guest_user_attributes, GuestIngressValidator, GuestRateLimiter, IngressDecision,
    DEFAULT_MAX_ATTRIBUTES, DEFAULT_MAX_ATTR_KEY_LEN, DEFAULT_MAX_ATTR_VAL_LEN,
    DEFAULT_MAX_INLINE_BYTES,
};
pub use hypervisor::{
    parse_firecracker_metrics, record_firecracker_metrics, BlockMetrics,
    FirecrackerMetricsSnapshot, NetMetrics, VcpuMetrics, VsockMetrics,
};
pub use lineage::{
    build_drive9_dataset, build_knowledge_input_dataset, OpenLineageDataset, OpenLineageJob,
    OpenLineageRun, OpenLineageRunEvent, OPENLINEAGE_PRODUCER, OPENLINEAGE_SCHEMA_URL,
};
pub use metrics::*;
pub use ocsf::{
    from_admission_reject, from_attestation_failure, from_cedar_decision, from_dns_block,
    from_egress_block, from_hitl_decision, from_secret_access, OcsfActor, OcsfConnectionInfo,
    OcsfDnsQuery, OcsfEvent, OcsfFinding, OcsfMetadata,
};
pub use otlp::{
    apply_genai_aliases, endpoint_is_loopback, parse_enabled as parse_otlp_enabled, shutdown_otlp,
    OtlpConfig, DEFAULT_OTLP_ENDPOINT, ENV_OTLP_ENABLED, ENV_OTLP_ENDPOINT, ENV_OTLP_SLOW_MS,
    ENV_OTLP_SUCCESS_SAMPLE_RATE, OTLP_DISABLED_REMOTE_REQUIRES_MTLS,
};
pub use queue::{DiskSpoolQueue, LossyRingBuffer};
pub use redact::{DefaultRedactor, TelemetryRedactor};
pub use replay::{
    compare_trajectories, is_trajectory_replay_type, parse_trajectory_replay,
    sanitize_trajectory_rows, TrajectoryCompare, TrajectoryReplay, TrajectoryReplayEvent,
    TRAJECTORY_REPLAY_EVENT_TYPES,
};
pub use retention::{
    classify_spool_relpath, install_retention_manager, retention_manager, sweep_tenant_spool,
    RetentionClass, RetentionManager, RetentionPolicy, SweepReport, ENV_SPOOL_MAX_A_BYTES,
    ENV_SPOOL_MAX_B_BYTES, ENV_SPOOL_MAX_C_BYTES, ENV_SPOOL_MAX_D_BYTES,
};
pub use sampling::TailSamplingPolicy;
pub use semconv::{
    allowed_metric_label_keys, export_genai_alias, map_vam_to_genai, validate_metric_label,
    OTEL_GENAI_SEMCONV_VERSION, OTEL_SEMCONV_VERSION, VAM_TENANT_ID, VAM_USER_ID,
};
pub use slo::{report_slo_observation, CapacityMetrics, SloAlert, SloEvaluator, SloTier};
pub use subscriber::{init_telemetry, init_telemetry_with_otlp, shutdown_telemetry};
pub use tenant::{
    current_audit_partition, current_tenant_id, install_tenant_context, resolve_tenant_spool_dir,
    CollectorTlsConfig, TenantContext, TenantId, TenantSpoolManager, DEFAULT_TENANT_ID,
    ENV_OTLP_SPIFFE_ID, ENV_OTLP_TLS_CA, ENV_OTLP_TLS_CERT, ENV_OTLP_TLS_KEY, ENV_TENANT_ID,
};
pub use trace_context::{
    apply_inbound_parent, create_child_trace_context, create_trace_context, inbound_baggage,
    inbound_traceparent, inbound_tracestate, remote_parent_context, stamp_host_user_id,
    strip_guest_user_id, validate_traceparent, BaggageValidator, Traceparent,
};
