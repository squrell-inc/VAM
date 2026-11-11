//! Assemble a portable run evidence pack from the outbox + in-memory audit ledger.
//!
//! Trajectory and denial files keep types + digests only (T21/T23). A missing
//! `vam.v1.run.attested` row means the run is still in flight — callers get 409.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use vam_spec::v1::SignedEnvelope;
use vam_telemetry::{
    finish_pack, is_deny_disposition, sha256_hex, to_jsonl, wrap_attestation_dsse, ArtifactsPack,
    AuditRecord, DenialRow, EvidencePackResponse, RunOutcomeAttestation, TrajectoryPack,
    TrajectoryPackEvent, FILE_ARTIFACTS, FILE_ATTESTATION, FILE_ATTESTATION_DSSE,
    FILE_ATTESTATION_ENVELOPE, FILE_AUDIT, FILE_DENIALS, FILE_RECEIPT, FILE_TRAJECTORY,
};

use crate::error::{OrchestratorError, OutboxEvent};
use crate::trajectory::is_trajectory_event;
use crate::workflow::SqliteWorkflowStore;

const EVENT_RUN_ATTESTED: &str = "vam.v1.run.attested";
const EVENT_POLICY_DECIDED: &str = "vam.v1.policy.decided";
const EVENT_EGRESS_DECIDED: &str = "vam.v1.egress.decided";
const EVENT_EGRESS_BLOCKED: &str = "vam.v1.egress.blocked";

pub fn assemble_evidence_pack(
    store: &SqliteWorkflowStore,
    run_id: &str,
) -> Result<EvidencePackResponse, OrchestratorError> {
    let _state = store.get_run_state(run_id)?;
    let events = store.read_outbox_for_run(run_id, 0)?;
    let attested = events
        .iter()
        .find(|e| e.event_type == EVENT_RUN_ATTESTED)
        .ok_or_else(|| {
            OrchestratorError::Conflict(format!(
                "run '{run_id}' has no attestation; export after the run finishes"
            ))
        })?;

    let (mut attestation, envelope) = parse_attested_event(attested)?;
    if !attestation.goal.is_empty() && !attestation.goal.starts_with("sha256:") {
        attestation.goal = format!("sha256:{}", sha256_hex(attestation.goal.as_bytes()));
    }
    let attestation_json = serde_json::to_string(&attestation).map_err(|e| {
        OrchestratorError::Core(format!("failed to serialize attestation.json: {e}"))
    })?;
    let dsse = wrap_attestation_dsse(attestation_json.as_bytes(), &envelope)
        .map_err(|e| OrchestratorError::Core(format!("failed to wrap attestation DSSE: {e}")))?;
    let dsse_json = serde_json::to_string(&dsse)
        .map_err(|e| OrchestratorError::Core(format!("failed to serialize DSSE: {e}")))?;

    let session_id = store
        .get_run_session_id(run_id)?
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| attestation.session_id.clone());
    let audit_partition = vam_telemetry::current_audit_partition(&session_id);
    let audit_records = store
        .audit_ledger()
        .map(|ledger| {
            let tenanted = ledger.read_partition(&audit_partition);
            if tenanted.is_empty() {
                ledger.read_partition(&session_id)
            } else {
                tenanted
            }
        })
        .unwrap_or_default();
    let audit_jsonl = to_jsonl(&audit_records)
        .map_err(|e| OrchestratorError::Core(format!("failed to serialize audit.jsonl: {e}")))?;

    let trajectory = TrajectoryPack {
        digest: attestation.trajectory_digest.clone(),
        events: events
            .iter()
            .filter(|e| is_trajectory_event(&e.event_type))
            .map(|e| TrajectoryPackEvent {
                sequence: e.sequence,
                event_type: e.event_type.clone(),
            })
            .collect(),
    };
    let trajectory_json = serde_json::to_string(&trajectory).map_err(|e| {
        OrchestratorError::Core(format!("failed to serialize trajectory.json: {e}"))
    })?;

    let artifacts = ArtifactsPack {
        artifacts: attestation.artifacts.clone(),
        paths: attestation.paths.clone(),
        trajectory_digest: attestation.trajectory_digest.clone(),
    };
    let artifacts_json = serde_json::to_string(&artifacts)
        .map_err(|e| OrchestratorError::Core(format!("failed to serialize artifacts.json: {e}")))?;

    let denials = collect_denials(&events);
    let denials_jsonl = to_jsonl(&denials)
        .map_err(|e| OrchestratorError::Core(format!("failed to serialize denials.jsonl: {e}")))?;

    let mut files = BTreeMap::new();
    files.insert(FILE_ATTESTATION.to_string(), attestation_json);
    files.insert(FILE_ATTESTATION_DSSE.to_string(), dsse_json);
    files.insert(FILE_AUDIT.to_string(), audit_jsonl);
    files.insert(FILE_TRAJECTORY.to_string(), trajectory_json);
    files.insert(FILE_ARTIFACTS.to_string(), artifacts_json);
    files.insert(FILE_DENIALS.to_string(), denials_jsonl);
    // The signed attestation as stored, so the pack can check its signature
    // against the node key the receipt names.
    files.insert(
        FILE_ATTESTATION_ENVELOPE.to_string(),
        attested.payload.clone(),
    );
    // The stored receipt, byte for byte. A run that has none (it ended before
    // receipts, or the key was missing) gets a pack without one.
    if let Some(stored) = store.run_receipt(run_id)? {
        files.insert(FILE_RECEIPT.to_string(), stored.document);
    }

    let exported_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    finish_pack(run_id, exported_at_unix_ms, files)
        .map_err(|e| OrchestratorError::Core(e.to_string()))
}

fn parse_attested_event(
    event: &OutboxEvent,
) -> Result<(RunOutcomeAttestation, SignedEnvelope), OrchestratorError> {
    let envelope: SignedEnvelope =
        vam_spec::encoding::json_to_proto("vam.v1.SignedEnvelope", &event.payload).map_err(
            |e| OrchestratorError::Core(format!("invalid vam.v1.run.attested envelope: {e}")),
        )?;
    let attestation: RunOutcomeAttestation = serde_json::from_slice(&envelope.claims)
        .map_err(|e| OrchestratorError::Core(format!("invalid attestation claims: {e}")))?;
    Ok((attestation, envelope))
}

fn collect_denials(events: &[OutboxEvent]) -> Vec<DenialRow> {
    let mut rows = Vec::new();
    for event in events {
        if !is_denial_event_type(&event.event_type) {
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<serde_json::Value>(&event.payload) else {
            continue;
        };
        // A CloudEvent keeps its fields under `data`; a bare payload carries them at the top.
        let payload = envelope
            .get("data")
            .filter(|data| data.is_object())
            .unwrap_or(&envelope);
        let disposition = payload
            .get("disposition")
            .or_else(|| payload.get("decision"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !is_deny_disposition(disposition) {
            continue;
        }
        rows.push(DenialRow {
            event_type: event.event_type.clone(),
            disposition: "DENY".to_string(),
            tool_name: payload
                .get("tool_name")
                .or_else(|| payload.get("toolName"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            args_digest: payload
                .get("args_digest")
                .or_else(|| payload.get("argsDigest"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            domain_digest: payload
                .get("domain_digest")
                .or_else(|| payload.get("domainDigest"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    payload
                        .get("domain")
                        .and_then(|v| v.as_str())
                        .map(|d| format!("sha256:{}", sha256_hex(d.as_bytes())))
                }),
        });
    }
    rows
}

fn is_denial_event_type(event_type: &str) -> bool {
    matches!(
        event_type,
        EVENT_POLICY_DECIDED | EVENT_EGRESS_DECIDED | EVENT_EGRESS_BLOCKED
    )
}

/// Optional crash-survive copy of an in-memory audit record (not a durable ledger).
pub fn append_audit_jsonl(spool_dir: &Path, record: &AuditRecord) -> std::io::Result<()> {
    let dir = spool_dir.join("audit");
    fs::create_dir_all(&dir)?;
    let safe: String = record
        .partition_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{safe}.jsonl"));
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writeln!(file, "{line}")
}

pub fn persist_audit_record_to_spool(record: &AuditRecord) {
    let Some(tenant_spool) = vam_telemetry::resolve_tenant_spool_dir(None) else {
        return;
    };
    let line = match serde_json::to_string(record) {
        Ok(l) => l,
        Err(_) => return,
    };
    let extra = (line.len() as u64).saturating_add(1);
    let audit_dir = tenant_spool.join("audit");
    let current = dir_size(&audit_dir);
    if let Some(mgr) = vam_telemetry::retention_manager() {
        if let Err(e) = mgr.check_quota(vam_telemetry::RetentionClass::ClassA_Audit, extra) {
            tracing::error!(
                error = %e,
                audit_dir = %audit_dir.display(),
                "Class A audit persist refused (fail-closed)"
            );
            return;
        }
        let _ = mgr.record_usage(vam_telemetry::RetentionClass::ClassA_Audit, extra);
    } else {
        let limit = vam_telemetry::RetentionPolicy::from_env().max_class_a_bytes;
        if current.saturating_add(extra) > limit {
            tracing::error!(
                usage_bytes = current.saturating_add(extra),
                limit_bytes = limit,
                audit_dir = %audit_dir.display(),
                "Class A audit persist refused (fail-closed)"
            );
            return;
        }
    }
    let _ = append_audit_jsonl(&tenant_spool, record);
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outbox(event_type: &str, payload: String) -> OutboxEvent {
        OutboxEvent {
            sequence: 1,
            run_id: "run-1".to_string(),
            event_type: event_type.to_string(),
            payload,
        }
    }

    #[test]
    fn collect_denials_reads_an_egress_cloud_event() {
        let deny = crate::events::egress_decided_json("run-1", 0, "evil.example", 443, "DENY", "p")
            .expect("deny event");
        let allow = crate::events::egress_decided_json("run-1", 0, "ok.example", 443, "ALLOW", "p")
            .expect("allow event");
        let events = vec![
            outbox(EVENT_EGRESS_DECIDED, deny),
            outbox(EVENT_EGRESS_DECIDED, allow),
        ];
        let rows = collect_denials(&events);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].event_type, EVENT_EGRESS_DECIDED);
        assert_eq!(rows[0].disposition, "DENY");
        assert!(rows[0].domain_digest.is_some(), "{rows:?}");
    }

    #[test]
    fn collect_denials_still_reads_a_bare_payload() {
        let events = vec![outbox(
            EVENT_POLICY_DECIDED,
            r#"{"disposition":"DENY","tool_name":"shell"}"#.to_string(),
        )];
        let rows = collect_denials(&events);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].tool_name.as_deref(), Some("shell"));
    }
}
