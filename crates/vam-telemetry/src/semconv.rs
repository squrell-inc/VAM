use crate::error::VamTelemetryError;
use std::collections::HashSet;

/// Pinned OpenTelemetry semantic conventions version.
pub const OTEL_SEMCONV_VERSION: &str = "1.28.0";

/// Pinned OpenTelemetry GenAI semantic conventions version (Development).
pub const OTEL_GENAI_SEMCONV_VERSION: &str = "development-2026";

// Standard VAM Attribute Keys (Internal namespace)
pub const VAM_SESSION_ID: &str = "vam.session.id";
pub const VAM_RUN_ID: &str = "vam.run.id";
pub const VAM_AGENT_URN: &str = "vam.agent.urn";
pub const VAM_TOOL_NAME: &str = "vam.tool.name";
pub const VAM_LEASE_ID: &str = "vam.lease.id";
pub const VAM_NETWORK_MODE: &str = "vam.network.mode";
pub const VAM_SECURITY_DISPOSITION: &str = "vam.security.disposition";
pub const VAM_MODEL_BILLED: &str = "vam.model.billed";
pub const VAM_TOKEN_PROMPT: &str = "vam.token.prompt";
pub const VAM_TOKEN_COMPLETION: &str = "vam.token.completion";
pub const VAM_THREAD_ID: &str = "vam.thread.id";
pub const VAM_LANE: &str = "vam.lane";
pub const VAM_CLOCK_DOMAIN: &str = "vam.clock_domain";
/// Host-stamped control-plane owner. Never taken from guest args or baggage.
pub const VAM_USER_ID: &str = "vam.user.id";
/// Telemetry/audit spool tenant (`VAMD_TENANT_ID`). Not a control-plane IAM id.
pub const VAM_TENANT_ID: &str = "vam.tenant.id";

/// Map internal VAM attributes to OpenTelemetry `gen_ai.*` conventions at the export edge.
pub fn map_vam_to_genai(vam_attr: &str) -> Option<&'static str> {
    match vam_attr {
        VAM_TOOL_NAME => Some("gen_ai.tool.name"),
        VAM_MODEL_BILLED => Some("gen_ai.response.model"),
        VAM_TOKEN_PROMPT => Some("gen_ai.usage.input_tokens"),
        VAM_TOKEN_COMPLETION => Some("gen_ai.usage.output_tokens"),
        VAM_THREAD_ID => Some("gen_ai.conversation.id"),
        _ => None,
    }
}

/// Export-edge aliases: `vam.*` via [`map_vam_to_genai`] plus a few tracing field
/// names (`thread_ref`, `model`). Never maps `vam.session.id` / `session_id`.
pub fn export_genai_alias(attr: &str) -> Option<&'static str> {
    if attr == VAM_SESSION_ID || attr == "session_id" {
        return None;
    }
    if let Some(mapped) = map_vam_to_genai(attr) {
        return Some(mapped);
    }
    match attr {
        "thread_ref" => Some("gen_ai.conversation.id"),
        "model" => Some("gen_ai.response.model"),
        _ => None,
    }
}

/// Allowed metric dimensions according to Low-Cardinality Label Policy (Decision T20).
pub fn allowed_metric_label_keys() -> HashSet<&'static str> {
    let mut s = HashSet::new();
    s.insert("network_mode");
    s.insert("model_billed");
    s.insert("admission_status");
    s.insert("hook_type");
    s.insert("hook_disposition");
    s.insert("error_category");
    s.insert("component");
    s.insert("status");
    // T20 exception: bounded broker identifier set
    s.insert("broker_id");
    s.insert("lane");
    s.insert("clock_domain");
    s.insert("exit_code");
    s.insert("tier");
    s.insert("direction");
    s.insert("subsystem");
    s.insert("result");
    s.insert("device");
    s
}

/// Validate metric dimension key and value for cardinality bounds (Decision T20).
pub fn validate_metric_label(key: &str, value: &str) -> Result<(), VamTelemetryError> {
    let allowed = allowed_metric_label_keys();
    let lower_key = key.to_ascii_lowercase();

    // Explicit rejection of known dangerous high-cardinality patterns
    if lower_key.contains("session")
        || lower_key.contains("user")
        || lower_key.contains("prompt")
        || lower_key.contains("path")
        || lower_key.contains("ip")
        || lower_key.contains("arg")
        || lower_key.contains("secret")
    {
        return Err(VamTelemetryError::InvalidLabel {
            key: key.to_string(),
            reason: "high-cardinality dimension is forbidden under Decision T20".to_string(),
        });
    }

    if !allowed.contains(lower_key.as_str()) {
        return Err(VamTelemetryError::InvalidLabel {
            key: key.to_string(),
            reason: format!(
                "metric dimension '{}' is not in the approved low-cardinality allowlist",
                key
            ),
        });
    }

    if value.len() > 64 {
        return Err(VamTelemetryError::InvalidLabel {
            key: key.to_string(),
            reason: format!(
                "metric label value exceeds 64 bytes (found {})",
                value.len()
            ),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_genai_mapping() {
        assert_eq!(map_vam_to_genai(VAM_TOOL_NAME), Some("gen_ai.tool.name"));
        assert_eq!(
            map_vam_to_genai(VAM_MODEL_BILLED),
            Some("gen_ai.response.model")
        );
        assert_eq!(map_vam_to_genai(VAM_SESSION_ID), None);
        assert_eq!(export_genai_alias(VAM_SESSION_ID), None);
        assert_eq!(export_genai_alias("session_id"), None);
        assert_eq!(
            export_genai_alias("thread_ref"),
            Some("gen_ai.conversation.id")
        );
        assert_eq!(export_genai_alias("model"), Some("gen_ai.response.model"));
        assert_eq!(export_genai_alias(VAM_TOOL_NAME), Some("gen_ai.tool.name"));
    }

    #[test]
    fn test_allowed_metric_labels() {
        assert!(validate_metric_label("network_mode", "restricted_direct").is_ok());
        assert!(validate_metric_label("model_billed", "gpt-4o").is_ok());
        assert!(validate_metric_label("admission_status", "allowed").is_ok());
        assert!(validate_metric_label("broker_id", "echo-broker").is_ok());
    }

    #[test]
    fn test_forbidden_high_cardinality_labels() {
        assert!(validate_metric_label("session_id", "sess-123").is_err());
        assert!(validate_metric_label("user_id", "usr-456").is_err());
        assert!(validate_metric_label("destination_ip", "1.2.3.4").is_err());
        assert!(validate_metric_label("prompt_text", "hello world").is_err());
        assert!(validate_metric_label("file_path", "/etc/passwd").is_err());
        assert!(validate_metric_label("custom_key", "value").is_err());
    }

    #[test]
    fn test_label_value_length_cap() {
        let long_val = "v".repeat(65);
        assert!(validate_metric_label("status", &long_val).is_err());
    }
}
