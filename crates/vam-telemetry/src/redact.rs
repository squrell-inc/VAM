use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

/// Zero-I/O Pre-Spool Redaction Trait (Decision T21).
pub trait TelemetryRedactor: Send + Sync {
    /// Redact sensitive PII and secrets in raw text strings.
    fn redact_text(&self, text: &str) -> String;

    /// Recursively redact in-place structured JSON values.
    fn redact_json(&self, val: &mut Value);
}

/// Standard Redactor with regex patterns for PII, Bearer tokens, and cloud API keys.
pub struct DefaultRedactor {
    bearer_regex: Regex,
    vam_key_regex: Regex,
    openai_key_regex: Regex,
    aws_key_regex: Regex,
    email_regex: Regex,
    ssn_regex: Regex,
    credit_card_regex: Regex,
    credential_kv_regex: Regex,
}

static DEFAULT_REDACTOR: OnceLock<DefaultRedactor> = OnceLock::new();

impl Default for DefaultRedactor {
    fn default() -> Self {
        Self {
            bearer_regex: Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9_\-\.]{10,}\b")
                .expect("bearer regex must compile"),
            vam_key_regex: Regex::new(r"\bvam_sk_[a-zA-Z0-9_-]{16,}\b")
                .expect("vam_key regex must compile"),
            openai_key_regex: Regex::new(r"\bsk-[a-zA-Z0-9_-]{20,}\b")
                .expect("openai_key regex must compile"),
            aws_key_regex: Regex::new(r"\bAKIA[0-9A-Z]{16}\b").expect("aws_key regex must compile"),
            email_regex: Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b")
                .expect("email regex must compile"),
            ssn_regex: Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").expect("ssn regex must compile"),
            credit_card_regex: Regex::new(r"\b(?:\d{4}[ -]?){3}\d{4}\b")
                .expect("credit_card regex must compile"),
            credential_kv_regex: Regex::new(
                r#"(?i)(password|secret|token|api_key|apikey)\s*[:=]\s*["']?([^"' \s,;]+)["']?"#,
            )
            .expect("credential_kv regex must compile"),
        }
    }
}

impl DefaultRedactor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn global() -> &'static Self {
        DEFAULT_REDACTOR.get_or_init(Self::new)
    }

    fn is_sensitive_key(key: &str) -> bool {
        let k = key.to_ascii_lowercase();
        k.contains("password")
            || k.contains("secret")
            || k.contains("token")
            || k.contains("api_key")
            || k.contains("apikey")
            || k.contains("authorization")
            || k.contains("credential")
    }
}

impl TelemetryRedactor for DefaultRedactor {
    fn redact_text(&self, text: &str) -> String {
        let mut result = text.to_string();

        result = self
            .bearer_regex
            .replace_all(&result, "Bearer [REDACTED_TOKEN]")
            .to_string();
        result = self
            .vam_key_regex
            .replace_all(&result, "[REDACTED_API_KEY]")
            .to_string();
        result = self
            .openai_key_regex
            .replace_all(&result, "[REDACTED_API_KEY]")
            .to_string();
        result = self
            .aws_key_regex
            .replace_all(&result, "[REDACTED_AWS_KEY]")
            .to_string();
        result = self
            .credential_kv_regex
            .replace_all(&result, "$1: [REDACTED_SECRET]")
            .to_string();
        result = self
            .ssn_regex
            .replace_all(&result, "[REDACTED_SSN]")
            .to_string();
        result = self
            .credit_card_regex
            .replace_all(&result, "[REDACTED_CARD]")
            .to_string();
        result = self
            .email_regex
            .replace_all(&result, "[REDACTED_EMAIL]")
            .to_string();

        result
    }

    fn redact_json(&self, val: &mut Value) {
        match val {
            Value::String(s) => {
                *s = self.redact_text(s);
            }
            Value::Array(arr) => {
                for item in arr.iter_mut() {
                    self.redact_json(item);
                }
            }
            Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    if Self::is_sensitive_key(k) {
                        *v = Value::String("[REDACTED_SECRET]".to_string());
                    } else {
                        self.redact_json(v);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_redact_tokens_and_keys() {
        let redactor = DefaultRedactor::new();

        let input = "Calling OpenAI with sk-1234567890abcdef1234567890abcdef and VAM key vam_sk_dev_18cd74a2a97ff4d1";
        let cleaned = redactor.redact_text(input);
        assert!(!cleaned.contains("vam_sk_dev_18cd74a2a97ff4d1"));
        assert!(!cleaned.contains("sk-1234567890abcdef1234567890abcdef"));
        assert!(cleaned.contains("[REDACTED_API_KEY]"));
    }

    #[test]
    fn test_redact_bearer_auth() {
        let redactor = DefaultRedactor::new();
        let input = "Authorization: Bearer my-secret-bearer-token-12345 in request";
        let cleaned = redactor.redact_text(input);
        assert!(!cleaned.contains("my-secret-bearer-token-12345"));
        assert!(cleaned.contains("Bearer [REDACTED_TOKEN]"));
    }

    #[test]
    fn test_redact_pii() {
        let redactor = DefaultRedactor::new();

        let email_text = "Contact user at john.doe@example.com for details";
        assert_eq!(
            redactor.redact_text(email_text),
            "Contact user at [REDACTED_EMAIL] for details"
        );

        let ssn_text = "SSN is 123-45-6789 on the form";
        assert_eq!(
            redactor.redact_text(ssn_text),
            "SSN is [REDACTED_SSN] on the form"
        );

        let card_text = "Card 4111-2222-3333-4444 charged successfully";
        assert_eq!(
            redactor.redact_text(card_text),
            "Card [REDACTED_CARD] charged successfully"
        );
    }

    #[test]
    fn test_redact_json_structure() {
        let redactor = DefaultRedactor::new();

        let mut payload = json!({
            "tool": "database_query",
            "api_key": "raw-key-value-123",
            "arguments": {
                "user_email": "alice@company.org",
                "notes": "Bearer secret-session-token-xyz"
            }
        });

        redactor.redact_json(&mut payload);

        assert_eq!(payload["api_key"], "[REDACTED_SECRET]");
        assert_eq!(payload["arguments"]["user_email"], "[REDACTED_EMAIL]");
        assert_eq!(payload["arguments"]["notes"], "Bearer [REDACTED_TOKEN]");
    }
}
