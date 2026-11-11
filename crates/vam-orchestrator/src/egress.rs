//! Standards egress worker: CloudEvents HTTP + spool, OCSF/FOCUS/DSSE/OpenLineage files.
//!
//! Outbox stays the system of record. HTTP and JSONL files are at-least-once projections.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use vam_spec::v1::SignedEnvelope;
use vam_telemetry::{
    cloud_event_to_vec, focus_from_cost_summary, maybe_mirror_otlp, parse_flag_default_false,
    project_outbox_event, unix_ms_to_rfc3339, wrap_attestation_dsse, write_attestation_jsonl,
    write_cloudevents_jsonl, write_focus_jsonl, write_lineage_jsonl, write_ocsf_jsonl,
    CloudEventJson, CostSummary, DiskSpoolQueue, OcsfEvent, CLOUDEVENTS_CONTENT_TYPE,
    ENV_CLOUDEVENTS_MIRROR_OTLP,
};

use crate::error::OutboxEvent;
use crate::events::VAMD_SOURCE_URN;
use crate::workflow::SqliteWorkflowStore;

pub const ENV_CLOUDEVENTS_URL: &str = "VAMD_CLOUDEVENTS_URL";
pub const ENV_SPOOL_DIR: &str = "VAMD_SPOOL_DIR";
pub const ENV_OPENLINEAGE_URL: &str = "VAMD_OPENLINEAGE_URL";
pub const DEFAULT_SPOOL_DIR: &str = "/var/lib/vam/spool";

const CE_SPOOL_FILE: &str = "cloudevents.spool";
const OL_SPOOL_FILE: &str = "openlineage.spool";
const CURSOR_FILE: &str = "cloudevents.cursor";
const MAX_SPOOL_BYTES: u64 = 64 * 1024 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

static INSTALLED: RwLock<Option<Arc<StandardsEgress>>> = RwLock::new(None);

#[derive(Debug, Clone)]
pub struct EgressConfig {
    pub cloudevents_url: String,
    pub spool_dir: PathBuf,
    pub mirror_otlp: bool,
    pub otlp_enabled: bool,
    pub openlineage_url: String,
}

impl EgressConfig {
    pub fn from_env(otlp_enabled: bool) -> Self {
        Self {
            cloudevents_url: std::env::var(ENV_CLOUDEVENTS_URL).unwrap_or_default(),
            spool_dir: std::env::var(ENV_SPOOL_DIR)
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_SPOOL_DIR)),
            mirror_otlp: parse_flag_default_false(
                std::env::var(ENV_CLOUDEVENTS_MIRROR_OTLP).ok().as_deref(),
            ),
            otlp_enabled,
            openlineage_url: std::env::var(ENV_OPENLINEAGE_URL).unwrap_or_default(),
        }
    }

    pub fn cloudevents_http_enabled(&self) -> bool {
        !self.cloudevents_url.is_empty()
    }

    pub fn openlineage_http_enabled(&self) -> bool {
        !self.openlineage_url.is_empty()
    }

    pub fn mirror_enabled(&self) -> bool {
        self.otlp_enabled && self.mirror_otlp
    }
}

#[derive(Debug)]
pub struct StandardsEgress {
    pub config: EgressConfig,
    ce_spool: DiskSpoolQueue,
    ol_spool: DiskSpoolQueue,
}

impl StandardsEgress {
    pub fn new(config: EgressConfig) -> Self {
        let _ = std::fs::create_dir_all(&config.spool_dir);
        let ce_spool =
            DiskSpoolQueue::new(config.spool_dir.join(CE_SPOOL_FILE), 0, MAX_SPOOL_BYTES);
        let ol_spool =
            DiskSpoolQueue::new(config.spool_dir.join(OL_SPOOL_FILE), 0, MAX_SPOOL_BYTES);
        Self {
            config,
            ce_spool,
            ol_spool,
        }
    }

    pub fn spool_dir(&self) -> &Path {
        &self.config.spool_dir
    }

    pub fn write_ocsf(&self, event: &OcsfEvent) -> Result<(), vam_telemetry::VamTelemetryError> {
        write_ocsf_jsonl(self.spool_dir(), event)
    }

    pub fn write_focus(
        &self,
        run_id: &str,
        cost: &CostSummary,
        start: &str,
        end: &str,
    ) -> Result<(), vam_telemetry::VamTelemetryError> {
        let record = focus_from_cost_summary(run_id, cost, start, end);
        write_focus_jsonl(self.spool_dir(), &record)
    }

    pub fn write_lineage_file(
        &self,
        lineage_json: &str,
    ) -> Result<(), vam_telemetry::VamTelemetryError> {
        write_lineage_jsonl(self.spool_dir(), lineage_json)
    }

    pub fn write_dsse(
        &self,
        attestation_json: &[u8],
        envelope: &SignedEnvelope,
    ) -> Result<(), vam_telemetry::VamTelemetryError> {
        let dsse = wrap_attestation_dsse(attestation_json, envelope)?;
        let value = vam_telemetry::dsse::dsse_to_value(&dsse)?;
        write_attestation_jsonl(self.spool_dir(), &value)
    }

    pub fn emit_lineage(&self, lineage_json: &str) {
        if let Err(e) = self.write_lineage_file(lineage_json) {
            tracing::warn!(error = %e, "failed to append lineage.jsonl (fail-open)");
        }
        if !self.config.openlineage_http_enabled() {
            return;
        }
        match post_json_blocking(
            &self.config.openlineage_url,
            "application/json",
            lineage_json.as_bytes(),
        ) {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(error = %e, "OpenLineage HTTP failed; spooling");
                if let Err(se) = self.ol_spool.enqueue(lineage_json.as_bytes().to_vec()) {
                    tracing::warn!(error = %se, "failed to enqueue OpenLineage spool");
                }
            }
        }
    }

    pub fn deliver_cloud_event(&self, event: &CloudEventJson, run_id: &str, sequence: i64) {
        if self.config.mirror_enabled() {
            maybe_mirror_otlp(true, &event.event_type, run_id, sequence);
        }

        if let Err(e) = write_cloudevents_jsonl(self.spool_dir(), event) {
            tracing::warn!(error = %e, "failed to append cloudevents.jsonl (fail-open)");
        }

        if !self.config.cloudevents_http_enabled() {
            return;
        }
        let body = match cloud_event_to_vec(event) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize CloudEvent JSON");
                return;
            }
        };
        match post_json_blocking(
            &self.config.cloudevents_url,
            CLOUDEVENTS_CONTENT_TYPE,
            &body,
        ) {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(error = %e, "CloudEvents HTTP failed; spooling");
                if let Err(se) = self.ce_spool.enqueue(body) {
                    tracing::warn!(error = %se, "failed to enqueue CloudEvents spool");
                }
            }
        }
    }

    pub fn drain_spools(&self) {
        drain_one(
            &self.ce_spool,
            self.config.cloudevents_url.as_str(),
            CLOUDEVENTS_CONTENT_TYPE,
        );
        drain_one(
            &self.ol_spool,
            self.config.openlineage_url.as_str(),
            "application/json",
        );
    }

    pub fn load_cursor(&self) -> i64 {
        load_wal_cursor(self.spool_dir())
    }

    pub fn save_cursor(&self, sequence: i64) {
        if let Err(e) = save_wal_cursor(self.spool_dir(), sequence) {
            tracing::warn!(error = %e, sequence, "failed to persist CloudEvents WAL cursor");
        }
    }
}

pub fn install_standards_egress(egress: Arc<StandardsEgress>) {
    if let Ok(mut lock) = INSTALLED.write() {
        *lock = Some(egress);
    }
}

pub fn standards_egress() -> Option<Arc<StandardsEgress>> {
    INSTALLED.read().ok().and_then(|g| g.clone())
}

pub fn emit_ocsf(event: &OcsfEvent) {
    if let Some(egress) = standards_egress() {
        if let Err(e) = egress.write_ocsf(event) {
            tracing::warn!(error = %e, "failed to append ocsf.jsonl (fail-open)");
        }
    }
}

pub fn load_wal_cursor(spool_dir: &Path) -> i64 {
    let path = spool_dir.join(CURSOR_FILE);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

pub fn save_wal_cursor(spool_dir: &Path, sequence: i64) -> Result<(), String> {
    let _ = std::fs::create_dir_all(spool_dir);
    std::fs::write(spool_dir.join(CURSOR_FILE), sequence.to_string()).map_err(|e| e.to_string())
}

fn drain_one(spool: &DiskSpoolQueue, url: &str, content_type: &str) {
    if url.is_empty() {
        return;
    }
    loop {
        match spool.dequeue() {
            Ok(Some(payload)) => match post_json_blocking(url, content_type, &payload) {
                Ok(()) => {
                    if let Err(e) = spool.ack() {
                        tracing::warn!(error = %e, "spool ack failed");
                        break;
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "spool drain POST failed; will retry");
                    break;
                }
            },
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "spool dequeue failed");
                break;
            }
        }
    }
}

fn post_json_blocking(url: &str, content_type: &str, body: &[u8]) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(body.to_vec())
        .send()
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {}", resp.status()))
    }
}

pub fn project_outbox_row(row: &OutboxEvent) -> CloudEventJson {
    project_outbox_event(
        &row.run_id,
        row.sequence,
        &row.event_type,
        &row.payload,
        VAMD_SOURCE_URN,
    )
}

/// Replay unread outbox rows (WAL cursor) and persist the last acked sequence.
pub fn drain_outbox_wal(store: &SqliteWorkflowStore, egress: &StandardsEgress, cursor: &mut i64) {
    let rows = match store.read_outbox(*cursor) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "outbox WAL read failed (fail-open)");
            return;
        }
    };
    for row in rows {
        let projected = project_outbox_row(&row);
        egress.deliver_cloud_event(&projected, &row.run_id, row.sequence);
        *cursor = row.sequence;
        egress.save_cursor(*cursor);
    }
}

/// Subscribe to the bus (wake) and poll the outbox WAL so restart replays unread rows.
pub async fn run_standards_egress(
    store: Arc<SqliteWorkflowStore>,
    bus: Arc<vam_telemetry::BroadcastEventBus>,
    egress: Arc<StandardsEgress>,
) {
    let mut cursor = egress.load_cursor();
    drain_outbox_wal(store.as_ref(), egress.as_ref(), &mut cursor);

    let mut subscriber = bus.subscribe();
    let mut wal_tick = tokio::time::interval(Duration::from_millis(500));
    let mut spool_tick = tokio::time::interval(Duration::from_secs(2));

    loop {
        tokio::select! {
            ev = subscriber.recv() => {
                match ev {
                    Ok(_) => {
                        drain_outbox_wal(store.as_ref(), egress.as_ref(), &mut cursor);
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "CloudEvents bus subscriber lagged; WAL poll continues");
                        subscriber = bus.subscribe();
                    }
                }
            }
            _ = wal_tick.tick() => {
                drain_outbox_wal(store.as_ref(), egress.as_ref(), &mut cursor);
            }
            _ = spool_tick.tick() => {
                egress.drain_spools();
            }
        }
    }
}

pub fn log_egress_startup(config: &EgressConfig) {
    if config.cloudevents_http_enabled() {
        tracing::info!(
            cloudevents_url = %config.cloudevents_url,
            spool_dir = %config.spool_dir.display(),
            mirror_otlp = config.mirror_enabled(),
            openlineage_url = %if config.openlineage_http_enabled() {
                config.openlineage_url.as_str()
            } else {
                ""
            },
            "vamd CloudEvents egress: HTTP + spool"
        );
    } else {
        tracing::info!(
            spool_dir = %config.spool_dir.display(),
            mirror_otlp = config.mirror_enabled(),
            "vamd CloudEvents egress: files only"
        );
    }
}

/// Shared ISO window for FOCUS stand-in records (attest/finalize).
pub fn focus_period_now() -> (String, String) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let start = unix_ms_to_rfc3339(now.saturating_sub(60_000));
    let end = unix_ms_to_rfc3339(now);
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_telemetry::DEFAULT_SOURCE_URN;

    #[test]
    fn mirror_env_defaults_off() {
        assert!(!parse_flag_default_false(None));
        let cfg = EgressConfig {
            cloudevents_url: String::new(),
            spool_dir: PathBuf::from("/tmp/vam-spool-test"),
            mirror_otlp: false,
            otlp_enabled: true,
            openlineage_url: String::new(),
        };
        assert!(!cfg.mirror_enabled());
        assert!(!cfg.cloudevents_http_enabled());
    }

    #[test]
    fn wal_cursor_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_wal_cursor(dir.path()), 0);
        save_wal_cursor(dir.path(), 42).unwrap();
        assert_eq!(load_wal_cursor(dir.path()), 42);
    }

    #[test]
    fn project_outbox_row_has_string_uris() {
        let row = OutboxEvent {
            sequence: 9,
            run_id: "urn:vam:run:r1".to_string(),
            event_type: "vam.v1.run.created".to_string(),
            payload: "created".to_string(),
        };
        let ev = project_outbox_row(&row);
        assert_eq!(ev.source, VAMD_SOURCE_URN);
        assert_eq!(ev.source, DEFAULT_SOURCE_URN);
        assert!(ev.source.starts_with("urn:"));
        assert_eq!(ev.subject, "urn:vam:run:r1");
        assert!(ev.dataschema.contains("vam.v1.run.created"));
    }
}
