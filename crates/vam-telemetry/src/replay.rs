//! Host-side CAS trajectory replay / compare.
//!
//! Events are digest + event-type / tool / disposition only. No stdout bodies,
//! prompts, or tool args (T21/T23).

use crate::error::VamTelemetryError;
use serde::{Deserialize, Serialize};

/// Host PEP trajectory types. Guest `tool.completed` / `failed` stay observational.
pub const TRAJECTORY_REPLAY_EVENT_TYPES: &[&str] = &[
    "vam.v1.tool.invoked",
    "vam.v1.policy.decided",
    "vam.v1.skill.loaded",
    "vam.v1.host_tool.completed",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryReplay {
    pub digest: String,
    pub events: Vec<TrajectoryReplayEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryReplayEvent {
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryCompare {
    pub equal: bool,
    pub only_a: Vec<TrajectoryReplayEvent>,
    pub only_b: Vec<TrajectoryReplayEvent>,
    pub digest_a: String,
    pub digest_b: String,
}

pub fn is_trajectory_replay_type(event_type: &str) -> bool {
    TRAJECTORY_REPLAY_EVENT_TYPES.contains(&event_type)
}

/// Parse a CAS replay blob (`Vec` of outbox rows) into a sanitized replay.
pub fn parse_trajectory_replay(
    digest: impl Into<String>,
    cas_bytes: &[u8],
) -> Result<TrajectoryReplay, VamTelemetryError> {
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(cas_bytes).map_err(|e| VamTelemetryError::EvidencePackInvalid {
            reason: format!("trajectory CAS blob is not JSON: {e}"),
        })?;
    Ok(TrajectoryReplay {
        digest: digest.into(),
        events: sanitize_trajectory_rows(&rows),
    })
}

pub fn sanitize_trajectory_rows(rows: &[serde_json::Value]) -> Vec<TrajectoryReplayEvent> {
    rows.iter().filter_map(sanitize_row).collect()
}

fn sanitize_row(row: &serde_json::Value) -> Option<TrajectoryReplayEvent> {
    let event_type = row
        .get("event_type")
        .or_else(|| row.get("eventType"))
        .and_then(|v| v.as_str())
        .filter(|t| is_trajectory_replay_type(t))?
        .to_string();
    let payload = row
        .get("payload")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let payload_obj = match payload {
        serde_json::Value::String(s) => serde_json::from_str(&s).unwrap_or(serde_json::Value::Null),
        other => other,
    };
    let tool_name = payload_obj
        .get("tool_name")
        .or_else(|| payload_obj.get("toolName"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let disposition = payload_obj
        .get("disposition")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(TrajectoryReplayEvent {
        event_type,
        tool_name,
        disposition,
    })
}

/// Compare two sanitized replays: event-type sequence, tool set, dispositions.
pub fn compare_trajectories(a: &TrajectoryReplay, b: &TrajectoryReplay) -> TrajectoryCompare {
    let prefix = a
        .events
        .iter()
        .zip(b.events.iter())
        .take_while(|(x, y)| x == y)
        .count();
    let only_a = a.events[prefix..].to_vec();
    let only_b = b.events[prefix..].to_vec();
    let tools_a = tool_set(&a.events);
    let tools_b = tool_set(&b.events);
    let disp_a = dispositions(&a.events);
    let disp_b = dispositions(&b.events);
    let types_a: Vec<&str> = a.events.iter().map(|e| e.event_type.as_str()).collect();
    let types_b: Vec<&str> = b.events.iter().map(|e| e.event_type.as_str()).collect();
    let equal = a.digest == b.digest
        || (only_a.is_empty()
            && only_b.is_empty()
            && tools_a == tools_b
            && disp_a == disp_b
            && types_a == types_b);
    TrajectoryCompare {
        equal,
        only_a,
        only_b,
        digest_a: a.digest.clone(),
        digest_b: b.digest.clone(),
    }
}

fn tool_set(events: &[TrajectoryReplayEvent]) -> std::collections::BTreeSet<&str> {
    events
        .iter()
        .filter_map(|e| e.tool_name.as_deref())
        .collect()
}

fn dispositions(events: &[TrajectoryReplayEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| e.disposition.as_deref())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        event_type: &str,
        tool: Option<&str>,
        disposition: Option<&str>,
    ) -> TrajectoryReplayEvent {
        TrajectoryReplayEvent {
            event_type: event_type.to_string(),
            tool_name: tool.map(str::to_string),
            disposition: disposition.map(str::to_string),
        }
    }

    #[test]
    fn parse_filters_to_pep_types_and_strips_bodies() {
        let blob = serde_json::json!([
            {
                "sequence": 1,
                "run_id": "run-1",
                "event_type": "vam.v1.tool.invoked",
                "payload": "{\"tool_name\":\"bash\",\"args_digest\":\"abc\",\"stdout\":\"SECRET\"}"
            },
            {
                "sequence": 2,
                "run_id": "run-1",
                "event_type": "vam.v1.policy.decided",
                "payload": {"tool_name": "bash", "disposition": "DENY", "args_digest": "abc"}
            },
            {
                "sequence": 3,
                "run_id": "run-1",
                "event_type": "vam.v1.tool.completed",
                "payload": {"stdout": "should drop"}
            }
        ]);
        let replay = parse_trajectory_replay("digest-a", &serde_json::to_vec(&blob).unwrap())
            .expect("parse");
        assert_eq!(replay.digest, "digest-a");
        assert_eq!(replay.events.len(), 2);
        assert_eq!(replay.events[0].tool_name.as_deref(), Some("bash"));
        assert_eq!(replay.events[1].disposition.as_deref(), Some("DENY"));
        let json = serde_json::to_string(&replay).unwrap();
        assert!(!json.contains("SECRET"));
        assert!(!json.contains("stdout"));
        assert!(!json.contains("args"));
    }

    #[test]
    fn compare_identical_is_equal_extra_deny_is_not() {
        let a = TrajectoryReplay {
            digest: "d1".into(),
            events: vec![
                event("vam.v1.tool.invoked", Some("bash"), None),
                event("vam.v1.policy.decided", Some("bash"), Some("ALLOW")),
            ],
        };
        let same = compare_trajectories(&a, &a);
        assert!(same.equal);
        assert!(same.only_a.is_empty());
        assert!(same.only_b.is_empty());

        let mut b = a.clone();
        b.digest = "d2".into();
        b.events
            .push(event("vam.v1.policy.decided", Some("bash"), Some("DENY")));
        let diff = compare_trajectories(&a, &b);
        assert!(!diff.equal);
        assert!(diff.only_a.is_empty());
        assert_eq!(diff.only_b.len(), 1);
        assert_eq!(diff.only_b[0].disposition.as_deref(), Some("DENY"));
    }
}
