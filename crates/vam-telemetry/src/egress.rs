//! CloudEvents 1.0 JSON projection, FOCUS cost records, and JSONL file writers.
//!
//! Internal `VamCloudEvent` keeps `ResourceId` for `source` / `subject`. Egress JSON
//! uses CloudEvents 1.0 **string** URIs plus `dataschema`. The outbox remains the
//! system of record; these projections are at-least-once and may duplicate.

use crate::attestation::CostSummary;
use crate::error::VamTelemetryError;
use crate::ocsf::OcsfEvent;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use vam_spec::v1::VamCloudEvent;

pub const CLOUDEVENTS_CONTENT_TYPE: &str = "application/cloudevents+json";
pub const DATASCHEMA_BASE: &str = "https://vam.dev/schemas";
pub const DEFAULT_SOURCE_URN: &str = "urn:vam:node:local.vam.dev/acme/vamd";
pub const CLOUDEVENTS_JSONL: &str = "cloudevents.jsonl";
pub const OCSF_JSONL: &str = "ocsf.jsonl";
pub const FOCUS_JSONL: &str = "focus.jsonl";
pub const LINEAGE_JSONL: &str = "lineage.jsonl";
pub const ATTESTATIONS_JSONL: &str = "attestations.jsonl";
pub const ENV_CLOUDEVENTS_MIRROR_OTLP: &str = "VAMD_CLOUDEVENTS_MIRROR_OTLP";

/// CloudEvents 1.0 structured-mode JSON (string `source` / `subject`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CloudEventJson {
    pub specversion: String,
    pub id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub time: String,
    pub source: String,
    pub subject: String,
    pub datacontenttype: String,
    pub dataschema: String,
    pub data: Value,
}

/// FOCUS-shaped cost stand-in (not a billed FOCUS 1.2 exporter).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FocusCostRecord {
    #[serde(rename = "BilledCost")]
    pub billed_cost: String,
    #[serde(rename = "BillingCurrency")]
    pub billing_currency: String,
    #[serde(rename = "ChargePeriodStart")]
    pub charge_period_start: String,
    #[serde(rename = "ChargePeriodEnd")]
    pub charge_period_end: String,
    #[serde(rename = "SkuId")]
    pub sku_id: String,
    #[serde(rename = "SkuPriceId")]
    pub sku_price_id: String,
    #[serde(rename = "Tags")]
    pub tags: FocusTags,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FocusTags {
    pub run_id: String,
}

pub fn dataschema_uri(event_type: &str) -> String {
    format!("{DATASCHEMA_BASE}/{event_type}.json")
}

/// Default-off flag (`1` / `true` / `on` / `yes`).
pub fn parse_flag_default_false(val: Option<&str>) -> bool {
    match val {
        None => false,
        Some(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        ),
    }
}

pub fn micro_to_decimal(micro: i64) -> String {
    let sign = if micro < 0 { "-" } else { "" };
    let abs = micro.unsigned_abs();
    let whole = abs / 1_000_000;
    let frac = abs % 1_000_000;
    format!("{sign}{whole}.{frac:06}")
}

pub fn focus_from_cost_summary(
    run_id: &str,
    cost: &CostSummary,
    charge_period_start: &str,
    charge_period_end: &str,
) -> FocusCostRecord {
    let sku = cost
        .pricing_catalog_version
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    FocusCostRecord {
        billed_cost: micro_to_decimal(cost.spend_micro),
        billing_currency: cost.currency.clone(),
        charge_period_start: charge_period_start.to_string(),
        charge_period_end: charge_period_end.to_string(),
        sku_id: sku.clone(),
        sku_price_id: sku,
        tags: FocusTags {
            run_id: run_id.to_string(),
        },
    }
}

pub fn project_cloud_event(
    id: &str,
    event_type: &str,
    time: &str,
    source: &str,
    subject: &str,
    data: Value,
) -> CloudEventJson {
    CloudEventJson {
        specversion: "1.0".to_string(),
        id: id.to_string(),
        event_type: event_type.to_string(),
        time: time.to_string(),
        source: source.to_string(),
        subject: subject.to_string(),
        datacontenttype: "application/json".to_string(),
        dataschema: dataschema_uri(event_type),
        data,
    }
}

/// Project an internal proto CloudEvent to egress JSON (string URIs, no nested ResourceId).
pub fn project_vam_cloud_event(event: &VamCloudEvent) -> CloudEventJson {
    let source = event
        .source
        .as_ref()
        .map(|r| r.urn.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_SOURCE_URN);
    let subject = event
        .subject
        .as_ref()
        .map(|r| r.urn.as_str())
        .or_else(|| event.run_id.as_ref().map(|r| r.urn.as_str()))
        .unwrap_or("");
    let time = event
        .time
        .as_ref()
        .map(|ts| format_rfc3339(ts.seconds, ts.nanos))
        .unwrap_or_else(|| format_rfc3339(0, 0));
    let data = event
        .data
        .as_ref()
        .map(|any| parse_payload_bytes(&any.value))
        .unwrap_or(Value::Null);

    project_cloud_event(&event.id, &event.r#type, &time, source, subject, data)
}

/// Project an outbox row. If `payload` is already proto-JSON of `VamCloudEvent`,
/// flatten nested `source`/`subject` ResourceIds; otherwise wrap the payload as `data`.
pub fn project_outbox_event(
    run_id: &str,
    sequence: i64,
    event_type: &str,
    payload: &str,
    source_urn: &str,
) -> CloudEventJson {
    if let Ok(obj) = serde_json::from_str::<Value>(payload) {
        if looks_like_proto_cloud_event(&obj) {
            return project_from_proto_json(&obj, run_id, source_urn);
        }
        return project_cloud_event(
            &format!("{run_id}:{sequence}"),
            event_type,
            &unix_ms_to_rfc3339(now_unix_ms()),
            source_urn,
            run_id,
            obj,
        );
    }

    let data = if payload.is_empty() {
        Value::Null
    } else {
        Value::String(payload.to_string())
    };
    project_cloud_event(
        &format!("{run_id}:{sequence}"),
        event_type,
        &unix_ms_to_rfc3339(now_unix_ms()),
        source_urn,
        run_id,
        data,
    )
}

fn looks_like_proto_cloud_event(obj: &Value) -> bool {
    let has_spec = obj.get("specVersion").is_some() || obj.get("spec_version").is_some();
    let has_source = obj.get("source").is_some();
    let has_type = obj.get("type").is_some();
    has_spec && has_source && has_type
}

fn project_from_proto_json(
    obj: &Value,
    fallback_run: &str,
    fallback_source: &str,
) -> CloudEventJson {
    let event_type = obj
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("vam.v1.unknown");
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("missing-id");
    let source =
        flatten_resource_id(obj.get("source")).unwrap_or_else(|| fallback_source.to_string());
    let subject = flatten_resource_id(obj.get("subject"))
        .or_else(|| flatten_resource_id(obj.get("runId")))
        .or_else(|| flatten_resource_id(obj.get("run_id")))
        .unwrap_or_else(|| fallback_run.to_string());
    let time = obj
        .get("time")
        .map(timestamp_json_to_rfc3339)
        .unwrap_or_else(|| unix_ms_to_rfc3339(now_unix_ms()));
    let data = obj.get("data").cloned().unwrap_or(Value::Null);
    let data = flatten_any_data(data);

    project_cloud_event(id, event_type, &time, &source, &subject, data)
}

fn flatten_resource_id(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(urn) = value.get("urn").and_then(Value::as_str) {
        if !urn.is_empty() {
            return Some(urn.to_string());
        }
    }
    value.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn flatten_any_data(data: Value) -> Value {
    match data {
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get("value") {
                if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                    return parsed;
                }
            }
            if map.contains_key("typeUrl") || map.contains_key("type_url") {
                if let Some(v) = map.get("value") {
                    if let Some(bytes) = decode_proto_bytes_json(v) {
                        return parse_payload_bytes(&bytes);
                    }
                }
                return json!({
                    "typeUrl": map.get("typeUrl").or_else(|| map.get("type_url")).cloned().unwrap_or(Value::Null)
                });
            }
            Value::Object(map)
        }
        other => other,
    }
}

fn decode_proto_bytes_json(value: &Value) -> Option<Vec<u8>> {
    if let Some(s) = value.as_str() {
        return Some(s.as_bytes().to_vec());
    }
    if let Some(arr) = value.as_array() {
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            out.push(item.as_u64()? as u8);
        }
        return Some(out);
    }
    None
}

fn parse_payload_bytes(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        return Value::Null;
    }
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        return v;
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            return v;
        }
        // Redaction-before-spool: never emit raw non-JSON bodies as event data.
        return json!({ "encoding": "opaque", "bytes": bytes.len() });
    }
    json!({ "encoding": "opaque", "bytes": bytes.len() })
}

fn timestamp_json_to_rfc3339(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        if s.contains('T') {
            return s.to_string();
        }
    }
    let secs = value
        .get("seconds")
        .and_then(Value::as_i64)
        .or_else(|| {
            value
                .get("seconds")
                .and_then(Value::as_u64)
                .map(|u| u as i64)
        })
        .unwrap_or(0);
    let nanos = value.get("nanos").and_then(Value::as_i64).unwrap_or(0) as i32;
    format_rfc3339(secs, nanos)
}

pub fn unix_ms_to_rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let nanos = (ms.rem_euclid(1000) * 1_000_000) as i32;
    format_rfc3339(secs, nanos)
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Howard Hinnant civil-from-days + time-of-day → RFC3339 UTC.
pub fn format_rfc3339(secs: i64, nanos: i32) -> String {
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400) as u32;
    let (y, m, d) = civil_from_days(days);
    let h = tod / 3600;
    let min = (tod % 3600) / 60;
    let s = tod % 60;
    let millis = (nanos.max(0) as u32) / 1_000_000;
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{millis:03}Z")
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

pub fn cloud_event_to_vec(event: &CloudEventJson) -> Result<Vec<u8>, VamTelemetryError> {
    serde_json::to_vec(event).map_err(|e| VamTelemetryError::SchemaMismatch {
        event_type: format!("cloudevents-json: {e}"),
    })
}

/// Optional OTLP-logs *mirror* (correlation only). Payload bodies stay off the log.
/// Fail-open: a tracing event; collectors must not be required.
pub fn maybe_mirror_otlp(enabled: bool, event_type: &str, run_id: &str, sequence: i64) {
    if !enabled {
        return;
    }
    tracing::info!(
        target: "vam.cloudevents",
        event_type,
        run_id,
        sequence,
        "cloudevents otlp mirror"
    );
}

pub fn append_jsonl(dir: &Path, filename: &str, value: &Value) -> Result<(), VamTelemetryError> {
    std::fs::create_dir_all(dir).map_err(|e| VamTelemetryError::SpoolDiskFailure {
        path: dir.display().to_string(),
        details: e.to_string(),
    })?;
    let path = dir.join(filename);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| VamTelemetryError::SpoolDiskFailure {
            path: path.display().to_string(),
            details: e.to_string(),
        })?;
    let line = serde_json::to_string(value).map_err(|e| VamTelemetryError::SpoolDiskFailure {
        path: path.display().to_string(),
        details: e.to_string(),
    })?;
    writeln!(file, "{line}").map_err(|e| VamTelemetryError::SpoolDiskFailure {
        path: path.display().to_string(),
        details: e.to_string(),
    })?;
    Ok(())
}

pub fn write_ocsf_jsonl(dir: &Path, event: &OcsfEvent) -> Result<(), VamTelemetryError> {
    let value = serde_json::to_value(event).map_err(|e| VamTelemetryError::OcsfMappingFailed {
        reason: e.to_string(),
    })?;
    append_jsonl(dir, OCSF_JSONL, &value)
}

pub fn write_focus_jsonl(dir: &Path, record: &FocusCostRecord) -> Result<(), VamTelemetryError> {
    let value = serde_json::to_value(record).map_err(|e| VamTelemetryError::SpoolDiskFailure {
        path: dir.display().to_string(),
        details: e.to_string(),
    })?;
    append_jsonl(dir, FOCUS_JSONL, &value)
}

pub fn write_lineage_jsonl(dir: &Path, lineage_json: &str) -> Result<(), VamTelemetryError> {
    let value: Value =
        serde_json::from_str(lineage_json).map_err(|e| VamTelemetryError::LineageError {
            reason: e.to_string(),
        })?;
    append_jsonl(dir, LINEAGE_JSONL, &value)
}

pub fn write_cloudevents_jsonl(
    dir: &Path,
    event: &CloudEventJson,
) -> Result<(), VamTelemetryError> {
    let value = serde_json::to_value(event).map_err(|e| VamTelemetryError::SchemaMismatch {
        event_type: format!("cloudevents-jsonl: {e}"),
    })?;
    append_jsonl(dir, CLOUDEVENTS_JSONL, &value)
}

pub fn write_attestation_jsonl(dir: &Path, dsse: &Value) -> Result<(), VamTelemetryError> {
    append_jsonl(dir, ATTESTATIONS_JSONL, dsse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attestation::CostSummary;
    use vam_spec::v1::ResourceId;

    #[test]
    fn projection_uses_string_source_subject_and_dataschema() {
        let event = VamCloudEvent {
            id: "evt-1".to_string(),
            source: Some(ResourceId {
                urn: "urn:vam:node:local.vam.dev/acme/vamd".to_string(),
            }),
            spec_version: "1.0".to_string(),
            r#type: "vam.v1.tool.completed".to_string(),
            time: Some(prost_types::Timestamp {
                seconds: 1_778_000_000,
                nanos: 0,
            }),
            subject: Some(ResourceId {
                urn: "urn:vam:run:local.vam.dev/acme/run-1".to_string(),
            }),
            data_content_type: "application/json".to_string(),
            data: Some(prost_types::Any {
                type_url: "type.googleapis.com/google.protobuf.Struct".to_string(),
                value: br#"{"tool_name":"run","exit_code":0}"#.to_vec(),
            }),
            run_id: Some(ResourceId {
                urn: "urn:vam:run:local.vam.dev/acme/run-1".to_string(),
            }),
            sequence_number: 7,
            trace_context: None,
        };

        let projected = project_vam_cloud_event(&event);
        let json = serde_json::to_value(&projected).expect("json");
        assert_eq!(json["specversion"], "1.0");
        assert_eq!(json["source"], "urn:vam:node:local.vam.dev/acme/vamd");
        assert_eq!(json["subject"], "urn:vam:run:local.vam.dev/acme/run-1");
        assert!(json["source"].is_string());
        assert!(json["subject"].is_string());
        assert!(
            !json["source"].is_object(),
            "nested ResourceId must not leak"
        );
        assert_eq!(
            json["dataschema"],
            "https://vam.dev/schemas/vam.v1.tool.completed.json"
        );
        assert_eq!(json["data"]["tool_name"], "run");
        assert_eq!(json["type"], "vam.v1.tool.completed");
    }

    #[test]
    fn proto_json_outbox_flattens_resource_id() {
        let payload = r#"{
            "id":"18f9",
            "source":{"urn":"urn:vam:node:local.vam.dev/acme/vamd"},
            "specVersion":"1.0",
            "type":"vam.v1.inference.completed",
            "subject":{"urn":"urn:vam:run:r1/inference/m"},
            "data":{"model_name":"demo"}
        }"#;
        let projected = project_outbox_event(
            "urn:vam:run:r1",
            3,
            "vam.v1.inference.completed",
            payload,
            DEFAULT_SOURCE_URN,
        );
        let json = serde_json::to_value(&projected).expect("json");
        assert_eq!(json["source"], DEFAULT_SOURCE_URN);
        assert!(json["source"].is_string());
        assert!(!json["source"].is_object());
        assert!(json["dataschema"].as_str().unwrap().contains("inference"));
    }

    #[test]
    fn focus_from_cost_uses_micro_decimal() {
        let cost = CostSummary {
            currency: "USD".to_string(),
            spend_micro: 450_000,
            prompt_tokens: 10,
            completion_tokens: 2,
            total_tokens: 12,
            pricing_catalog_version: Some("2026-v1".to_string()),
        };
        let rec = focus_from_cost_summary(
            "urn:vam:run:r1",
            &cost,
            "2026-09-10T00:00:00.000Z",
            "2026-09-10T00:01:00.000Z",
        );
        assert_eq!(rec.billed_cost, "0.450000");
        assert_eq!(rec.billing_currency, "USD");
        assert_eq!(rec.sku_id, "2026-v1");
        assert_eq!(rec.tags.run_id, "urn:vam:run:r1");

        let dir = tempfile::tempdir().unwrap();
        write_focus_jsonl(dir.path(), &rec).expect("write");
        let line = std::fs::read_to_string(dir.path().join(FOCUS_JSONL)).unwrap();
        assert!(line.contains("\"BilledCost\":\"0.450000\""));
        assert!(line.contains("\"Tags\""));
    }

    #[test]
    fn ocsf_jsonl_writes_deny_line() {
        let event = crate::ocsf::from_cedar_decision("run-1", "sess-1", "bash", "DENY", "digest");
        let dir = tempfile::tempdir().unwrap();
        write_ocsf_jsonl(dir.path(), &event).expect("write");
        let line = std::fs::read_to_string(dir.path().join(OCSF_JSONL)).unwrap();
        assert!(line.contains("\"class_uid\":1007"));
        assert!(line.contains("\"disposition\":\"DENY\""));
    }

    #[test]
    fn mirror_flag_defaults_off() {
        assert!(!parse_flag_default_false(None));
        assert!(!parse_flag_default_false(Some("")));
        assert!(!parse_flag_default_false(Some("0")));
        assert!(!parse_flag_default_false(Some("false")));
        assert!(parse_flag_default_false(Some("1")));
        assert!(parse_flag_default_false(Some("true")));
    }

    #[test]
    fn rfc3339_formats_known_instant() {
        assert_eq!(format_rfc3339(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            unix_ms_to_rfc3339(1_778_000_000_000),
            format_rfc3339(1_778_000_000, 0)
        );
    }
}
