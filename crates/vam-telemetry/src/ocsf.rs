//! OCSF (Open Cybersecurity Schema Framework) v1.2.0 mappings for VAM security lane (Phase 7.1).

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const OCSF_VERSION: &str = "1.2.0";
pub const VAM_PRODUCT_NAME: &str = "Virtual Agentic Machine";
pub const VAM_VENDOR_NAME: &str = "VAM Project";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OcsfProduct {
    pub name: String,
    pub vendor_name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OcsfMetadata {
    pub version: String,
    pub product: OcsfProduct,
}

impl Default for OcsfMetadata {
    fn default() -> Self {
        Self {
            version: OCSF_VERSION.to_string(),
            product: OcsfProduct {
                name: VAM_PRODUCT_NAME.to_string(),
                vendor_name: VAM_VENDOR_NAME.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OcsfActor {
    pub user_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OcsfEndpoint {
    pub ip: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OcsfConnectionInfo {
    pub dst_endpoint: OcsfEndpoint,
    pub protocol_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OcsfDnsQuery {
    pub hostname: String,
    pub record_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OcsfFinding {
    pub title: String,
    pub desc: String,
}

/// Normalized OCSF Security Event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OcsfEvent {
    pub activity_id: u32,
    pub category_uid: u32,
    pub class_uid: u32,
    pub type_uid: u64,
    pub severity_id: u32,
    pub status_id: u32,
    pub disposition: String,
    pub message: String,
    pub time: u64,
    pub metadata: OcsfMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<OcsfActor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_info: Option<OcsfConnectionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_query: Option<OcsfDnsQuery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding: Option<OcsfFinding>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub unmapped: serde_json::Map<String, serde_json::Value>,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Maps a Cedar policy decision (e.g. authorization denial or grant) to OCSF Class 1007 (Authorize Session).
pub fn from_cedar_decision(
    run_id: &str,
    session_id: &str,
    tool_name: &str,
    disposition: &str,
    args_digest: &str,
) -> OcsfEvent {
    let is_deny = disposition.eq_ignore_ascii_case("DENY");
    let mut unmapped = serde_json::Map::new();
    unmapped.insert("run_id".to_string(), serde_json::json!(run_id));
    unmapped.insert("tool_name".to_string(), serde_json::json!(tool_name));
    unmapped.insert("args_digest".to_string(), serde_json::json!(args_digest));

    OcsfEvent {
        activity_id: if is_deny { 2 } else { 1 },
        category_uid: 1, // System Activity
        class_uid: 1007, // Authorize Session
        type_uid: 1007 * 100 + if is_deny { 2 } else { 1 },
        severity_id: if is_deny { 3 } else { 1 }, // Medium for Deny, Info for Allow
        status_id: if is_deny { 2 } else { 1 },   // Failure or Success
        disposition: disposition.to_string(),
        message: format!(
            "Cedar policy evaluated for tool '{}': {}",
            tool_name, disposition
        ),
        time: now_millis(),
        metadata: OcsfMetadata::default(),
        actor: Some(OcsfActor {
            user_name: run_id.to_string(),
            session_id: Some(session_id.to_string()),
        }),
        connection_info: None,
        dns_query: None,
        finding: None,
        unmapped,
    }
}

/// Maps an egress block (blocked packet/connection) to OCSF Class 4001 (Network Activity).
pub fn from_egress_block(session_id: &str, dest_ip: &str, port: u16, reason: &str) -> OcsfEvent {
    let mut unmapped = serde_json::Map::new();
    unmapped.insert("session_id".to_string(), serde_json::json!(session_id));
    unmapped.insert("reason".to_string(), serde_json::json!(reason));

    OcsfEvent {
        activity_id: 2,  // Traffic Denied
        category_uid: 4, // Network Activity
        class_uid: 4001, // Network Activity
        type_uid: 400102,
        severity_id: 3, // Medium
        status_id: 2,   // Failure
        disposition: "Block".to_string(),
        message: format!(
            "Egress connection to {}:{} blocked: {}",
            dest_ip, port, reason
        ),
        time: now_millis(),
        metadata: OcsfMetadata::default(),
        actor: Some(OcsfActor {
            user_name: "guest_microvm".to_string(),
            session_id: Some(session_id.to_string()),
        }),
        connection_info: Some(OcsfConnectionInfo {
            dst_endpoint: OcsfEndpoint {
                ip: dest_ip.to_string(),
                port,
            },
            protocol_name: "TCP".to_string(),
        }),
        dns_query: None,
        finding: None,
        unmapped,
    }
}

/// Maps a DNS resolution block to OCSF Class 4003 (DNS Activity).
pub fn from_dns_block(session_id: &str, domain: &str, reason: &str) -> OcsfEvent {
    let mut unmapped = serde_json::Map::new();
    unmapped.insert("session_id".to_string(), serde_json::json!(session_id));
    unmapped.insert("reason".to_string(), serde_json::json!(reason));

    OcsfEvent {
        activity_id: 2,  // Query Refused / Denied
        category_uid: 4, // Network Activity
        class_uid: 4003, // DNS Activity
        type_uid: 400302,
        severity_id: 3, // Medium
        status_id: 2,   // Failure
        disposition: "Block".to_string(),
        message: format!("DNS resolution for '{}' blocked: {}", domain, reason),
        time: now_millis(),
        metadata: OcsfMetadata::default(),
        actor: Some(OcsfActor {
            user_name: "guest_microvm".to_string(),
            session_id: Some(session_id.to_string()),
        }),
        connection_info: None,
        dns_query: Some(OcsfDnsQuery {
            hostname: domain.to_string(),
            record_type: "A".to_string(),
        }),
        finding: None,
        unmapped,
    }
}

/// Maps a secret access event to OCSF Class 1001 (Resource / Secret Access).
pub fn from_secret_access(session_id: &str, secret_name: &str, caller_urn: &str) -> OcsfEvent {
    let mut unmapped = serde_json::Map::new();
    unmapped.insert("secret_name".to_string(), serde_json::json!(secret_name));
    unmapped.insert("session_id".to_string(), serde_json::json!(session_id));

    OcsfEvent {
        activity_id: 1,  // Access Granted
        category_uid: 1, // System Activity
        class_uid: 1001, // Resource / Secret Access
        type_uid: 100101,
        severity_id: 1, // Informational
        status_id: 1,   // Success
        disposition: "Allow".to_string(),
        message: format!("Secret '{}' accessed by {}", secret_name, caller_urn),
        time: now_millis(),
        metadata: OcsfMetadata::default(),
        actor: Some(OcsfActor {
            user_name: caller_urn.to_string(),
            session_id: Some(session_id.to_string()),
        }),
        connection_info: None,
        dns_query: None,
        finding: None,
        unmapped,
    }
}

/// Maps an admission rejection (untrusted package, forged traceparent) to OCSF Class 2001 (Security Finding).
pub fn from_admission_reject(session_id: &str, reason: &str) -> OcsfEvent {
    let mut unmapped = serde_json::Map::new();
    unmapped.insert("session_id".to_string(), serde_json::json!(session_id));

    OcsfEvent {
        activity_id: 1,  // Finding Created
        category_uid: 2, // Findings
        class_uid: 2001, // Security Finding
        type_uid: 200101,
        severity_id: 4, // High
        status_id: 2,   // Failure
        disposition: "Reject".to_string(),
        message: format!(
            "Sandbox admission rejected for session '{}': {}",
            session_id, reason
        ),
        time: now_millis(),
        metadata: OcsfMetadata::default(),
        actor: Some(OcsfActor {
            user_name: "admission_controller".to_string(),
            session_id: Some(session_id.to_string()),
        }),
        connection_info: None,
        dns_query: None,
        finding: Some(OcsfFinding {
            title: "Sandbox Admission Rejected".to_string(),
            desc: reason.to_string(),
        }),
        unmapped,
    }
}

/// Maps an outcome attestation verification failure to OCSF Class 2001 (Security Finding).
pub fn from_attestation_failure(run_id: &str, session_id: &str, reason: &str) -> OcsfEvent {
    let mut unmapped = serde_json::Map::new();
    unmapped.insert("run_id".to_string(), serde_json::json!(run_id));
    unmapped.insert("session_id".to_string(), serde_json::json!(session_id));

    OcsfEvent {
        activity_id: 1,  // Finding Created
        category_uid: 2, // Findings
        class_uid: 2001, // Security Finding
        type_uid: 200101,
        severity_id: 5, // Critical
        status_id: 2,   // Failure
        disposition: "Reject".to_string(),
        message: format!(
            "Outcome attestation failed verification for run '{}': {}",
            run_id, reason
        ),
        time: now_millis(),
        metadata: OcsfMetadata::default(),
        actor: Some(OcsfActor {
            user_name: run_id.to_string(),
            session_id: Some(session_id.to_string()),
        }),
        connection_info: None,
        dns_query: None,
        finding: Some(OcsfFinding {
            title: "Outcome Attestation Verification Failed".to_string(),
            desc: reason.to_string(),
        }),
        unmapped,
    }
}

/// Maps a HITL decision to OCSF Class 1002 (Approval / Authorize Activity).
pub fn from_hitl_decision(
    request_id: &str,
    run_id: &str,
    tool_name: &str,
    decision: &str,
    reviewer: &str,
) -> OcsfEvent {
    let is_rejected = decision.eq_ignore_ascii_case("REJECTED");
    let mut unmapped = serde_json::Map::new();
    unmapped.insert("request_id".to_string(), serde_json::json!(request_id));
    unmapped.insert("run_id".to_string(), serde_json::json!(run_id));
    unmapped.insert("tool_name".to_string(), serde_json::json!(tool_name));

    OcsfEvent {
        activity_id: 1,
        category_uid: 1, // System Activity
        class_uid: 1002, // Approval Activity
        type_uid: 100201,
        severity_id: if is_rejected { 3 } else { 2 },
        status_id: if is_rejected { 2 } else { 1 },
        disposition: decision.to_string(),
        message: format!(
            "HITL approval request '{}' decided as {} by {}",
            request_id, decision, reviewer
        ),
        time: now_millis(),
        metadata: OcsfMetadata::default(),
        actor: Some(OcsfActor {
            user_name: reviewer.to_string(),
            session_id: None,
        }),
        connection_info: None,
        dns_query: None,
        finding: None,
        unmapped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ocsf_cedar_denial() {
        let event = from_cedar_decision("run-1", "sess-1", "exec_cmd", "DENY", "digest-123");
        assert_eq!(event.class_uid, 1007);
        assert_eq!(event.category_uid, 1);
        assert_eq!(event.disposition, "DENY");
        assert_eq!(event.severity_id, 3);
        assert_eq!(event.status_id, 2);

        let json = serde_json::to_string(&event).expect("json");
        assert!(json.contains("\"class_uid\":1007"));
        assert!(json.contains("\"disposition\":\"DENY\""));
        assert!(json.contains("\"tool_name\":\"exec_cmd\""));
    }

    #[test]
    fn test_ocsf_egress_and_dns_blocks() {
        let egress = from_egress_block("sess-1", "10.0.0.5", 443, "Denied CIDR");
        assert_eq!(egress.class_uid, 4001);
        assert_eq!(egress.disposition, "Block");
        assert_eq!(
            egress.connection_info.as_ref().unwrap().dst_endpoint.port,
            443
        );

        let dns = from_dns_block("sess-1", "malicious.com", "Blacklisted Domain");
        assert_eq!(dns.class_uid, 4003);
        assert_eq!(dns.disposition, "Block");
        assert_eq!(dns.dns_query.as_ref().unwrap().hostname, "malicious.com");
    }

    #[test]
    fn test_ocsf_security_findings() {
        let admission = from_admission_reject("sess-1", "Forged traceparent header");
        assert_eq!(admission.class_uid, 2001);
        assert_eq!(admission.severity_id, 4);
        assert_eq!(
            admission.finding.as_ref().unwrap().title,
            "Sandbox Admission Rejected"
        );

        let attestation = from_attestation_failure("run-1", "sess-1", "Ed25519 signature mismatch");
        assert_eq!(attestation.class_uid, 2001);
        assert_eq!(attestation.severity_id, 5);
        assert_eq!(
            attestation.finding.as_ref().unwrap().title,
            "Outcome Attestation Verification Failed"
        );
    }
}
