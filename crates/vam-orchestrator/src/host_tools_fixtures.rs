//! Test-only host adapters for the E2E scenarios.
//!
//! No runner form can make a host tool emit an encoded secret or a very large reply, so the
//! scenarios need adapters that do. They are registered only on a node started with
//! `VAMD_E2E_MOCK_SCRIPTS` (the switch that also gates `mock-model`); an ordinary node has
//! neither of them. They are fixtures: the output is what a hostile or careless adapter could
//! return, and the host's redaction and frame cap are what the scenarios judge.

use std::sync::Arc;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use secrecy::ExposeSecret;

use crate::host_tools::{HostToolAdapter, HostToolContext, HostToolOutput, HostToolRegistry};

/// Adapter id of the fixture that prints the first required secret as it is.
pub const SECRET_ECHO_ADAPTER_ID: &str = "secret_echo";
/// Adapter id of the fixture that prints the first required secret in several forms.
pub const SECRET_FORMS_ADAPTER_ID: &str = "secret_forms";
/// Adapter id of the fixture that returns `config.bytes` of text.
pub const BULK_OUTPUT_ADAPTER_ID: &str = "bulk_output";
/// Adapter id of the fixture that plays a guest asking the egress proxy's port 55 for oversize
/// bodies (`process-guest` builds only).
#[cfg(feature = "process-guest")]
pub const EGRESS_PROBE_ADAPTER_ID: &str = "egress_probe";

/// Every form `secret_forms` can print, in the order it prints them by default.
pub const ALL_FORMS: &[&str] = &[
    "raw",
    "base64",
    "base64url",
    "hex",
    "percent",
    "json",
    "split",
    "basic",
    "skew",
];

/// The registry a node uses: the ordinary test adapters, plus the fixtures when
/// `mock_scripts_on` (the node loaded `VAMD_E2E_MOCK_SCRIPTS`).
pub fn registry_for_node(mock_scripts_on: bool) -> HostToolRegistry {
    let mut registry = HostToolRegistry::with_test_adapters();
    if mock_scripts_on {
        registry.register(Arc::new(SecretEchoAdapter));
        registry.register(Arc::new(SecretFormsAdapter));
        registry.register(Arc::new(BulkOutputAdapter));
        #[cfg(feature = "process-guest")]
        registry.register(Arc::new(EgressProbeAdapter));
    }
    registry
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn hex(value: &str) -> String {
    value.bytes().map(|b| format!("{b:02x}")).collect()
}

fn json_escaped(value: &str) -> String {
    let quoted = serde_json::to_string(value).unwrap_or_default();
    quoted
        .strip_prefix('"')
        .and_then(|q| q.strip_suffix('"'))
        .unwrap_or_default()
        .to_string()
}

/// One line `form=<value>` per requested form. An unknown form name is reported, not skipped.
pub fn forms_text(value: &str, forms: &[&str]) -> String {
    let mut lines = Vec::new();
    for form in forms {
        let encoded = match *form {
            "raw" => value.to_string(),
            "base64" => STANDARD.encode(value),
            "base64url" => URL_SAFE_NO_PAD.encode(value),
            "hex" => hex(value),
            "percent" => percent_encode(value),
            "json" => json_escaped(value),
            "split" => {
                let mid = value.len() / 2;
                let mid = (mid..=value.len())
                    .find(|i| value.is_char_boundary(*i))
                    .unwrap_or(value.len());
                format!("{}\n{}", &value[..mid], &value[mid..])
            }
            // The secret inside a larger base64 blob, as HTTP Basic auth carries it.
            "basic" => STANDARD.encode(format!("user:{value}")),
            // The same, shifted by one byte, so the base64 groups fall differently.
            "skew" => STANDARD.encode(format!("a{value}")),
            other => format!("(unknown form {other})"),
        };
        lines.push(format!("{form}={encoded}"));
    }
    lines.join("\n")
}

/// Reads the first required secret and echoes it raw. Only redaction stands between its
/// output and the guest, so it is registered on test nodes only.
struct SecretEchoAdapter;

impl HostToolAdapter for SecretEchoAdapter {
    fn adapter_id(&self) -> &str {
        SECRET_ECHO_ADAPTER_ID
    }

    fn invoke(&self, ctx: &HostToolContext<'_>) -> Result<HostToolOutput, String> {
        let vault = ctx.vault.ok_or_else(|| "vault unavailable".to_string())?;
        let requirement = ctx
            .required_secrets
            .first()
            .ok_or_else(|| "secret_echo requires required_secrets".to_string())?;
        let secret_name = requirement.name.as_str();
        let scope = crate::secret_requirements::RequirementScope::from_proto(requirement.scope)
            .unwrap_or(crate::secret_requirements::RequirementScope::Either);
        let value = vault
            .decrypt_secret_not_connection(secret_name)
            .map_err(|_| {
                crate::secret_requirements::missing_binding_error(secret_name, scope).to_string()
            })?;
        Ok(HostToolOutput {
            text: format!("secret[{}]={}", secret_name, value.expose_secret()),
            artifact: None,
        })
    }
}

/// Prints the first required secret in the forms `config.forms` lists (comma separated;
/// default: all). Only redaction stands between its output and the guest.
struct SecretFormsAdapter;

impl HostToolAdapter for SecretFormsAdapter {
    fn adapter_id(&self) -> &str {
        SECRET_FORMS_ADAPTER_ID
    }

    fn invoke(&self, ctx: &HostToolContext<'_>) -> Result<HostToolOutput, String> {
        let vault = ctx.vault.ok_or_else(|| "vault unavailable".to_string())?;
        let requirement = ctx
            .required_secrets
            .first()
            .ok_or_else(|| "secret_forms requires required_secrets".to_string())?;
        let value = vault
            .decrypt_secret_not_connection(requirement.name.as_str())
            .map_err(|_| format!("secret `{}` is not bound", requirement.name))?;
        let wanted = ctx.config.get("forms").map(String::as_str);
        let forms: Vec<&str> = match wanted {
            Some(list) => list
                .split(',')
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .collect(),
            None => ALL_FORMS.to_vec(),
        };
        Ok(HostToolOutput {
            text: forms_text(value.expose_secret(), &forms),
            artifact: None,
        })
    }
}

/// Returns `config.bytes` bytes of plain text (default 200000), to outgrow the vsock frame.
struct BulkOutputAdapter;

pub fn bulk_text(bytes: usize) -> String {
    let line = "bulk-output-line 0123456789abcdef0123456789abcdef\n";
    let mut text = line.repeat(bytes / line.len() + 1);
    text.truncate(bytes);
    text
}

impl HostToolAdapter for BulkOutputAdapter {
    fn adapter_id(&self) -> &str {
        BULK_OUTPUT_ADAPTER_ID
    }

    fn invoke(&self, ctx: &HostToolContext<'_>) -> Result<HostToolOutput, String> {
        let bytes = match ctx.config.get("bytes") {
            Some(raw) => raw
                .parse::<usize>()
                .map_err(|_| format!("bulk_output: config.bytes `{raw}` is not a number"))?,
            None => 200_000,
        };
        Ok(HostToolOutput {
            text: bulk_text(bytes),
            artifact: None,
        })
    }
}

/// Plays a guest that asks the node's TLS-terminating egress proxy (port 55) for bodies from the
/// fake origin, which builds each body from the egress secret it holds. Tool processes cannot open
/// that socket in process mode, so the host opens it. The reply is a verdict, not the bytes: the
/// host redacts tool output, which would hide a leak, so the adapter checks the raw bytes itself
/// and prints `leak-found` or `leak-none`.
#[cfg(feature = "process-guest")]
struct EgressProbeAdapter;

#[cfg(feature = "process-guest")]
impl HostToolAdapter for EgressProbeAdapter {
    fn adapter_id(&self) -> &str {
        EGRESS_PROBE_ADAPTER_ID
    }

    fn invoke(&self, ctx: &HostToolContext<'_>) -> Result<HostToolOutput, String> {
        use vam_network::proxy::fixture::probe_port55;

        let vault = ctx.vault.ok_or_else(|| "vault unavailable".to_string())?;
        let requirement = ctx
            .required_secrets
            .first()
            .ok_or_else(|| "egress_probe requires required_secrets".to_string())?;
        let value = vault
            .decrypt_secret(requirement.name.as_str())
            .map_err(|_| format!("secret `{}` is not bound", requirement.name))?;
        let placeholder = format!("vamsec_{}", requirement.name.replace(['/', '-'], "_"));
        let redactor =
            vam_security::secret_forms::SecretRedactor::new(&[value.expose_secret().as_bytes()]);
        let socket = std::path::PathBuf::from(format!(
            "{}_55",
            vam_core::jail::vsock_uds_path(&format!("vm-{}", ctx.session_id)).display()
        ));
        let authorization = format!("Bearer {placeholder}");
        let mut leaked = false;
        let mut lines = Vec::new();
        let asks: [(&str, usize, Option<&str>); 3] = [
            ("small", 100, Some(&authorization)),
            ("oversize", 200_000, Some(&authorization)),
            ("later", 200_000, None),
        ];
        for (label, size, auth) in asks {
            let reply = probe_port55(&socket, size, auth)?;
            leaked |= matches!(
                redactor.redact_bytes(&reply.raw),
                std::borrow::Cow::Owned(_)
            );
            let verdict = match (label, reply.status) {
                ("small", 200) => "ok".to_string(),
                (_, 502) => "refused".to_string(),
                (_, 200) => "served".to_string(),
                (_, status) => format!("other{status}"),
            };
            lines.push(format!("{label}-{verdict}"));
        }
        lines.push(if leaked { "leak-found" } else { "leak-none" }.to_string());
        Ok(HostToolOutput {
            text: lines.join("\n"),
            artifact: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn an_ordinary_node_has_neither_fixture() {
        let registry = registry_for_node(false);
        assert!(registry.get(SECRET_ECHO_ADAPTER_ID).is_none());
        assert!(registry.get(SECRET_FORMS_ADAPTER_ID).is_none());
        assert!(registry.get(BULK_OUTPUT_ADAPTER_ID).is_none());
        assert!(HostToolRegistry::default()
            .get(SECRET_ECHO_ADAPTER_ID)
            .is_none());
        // The ordinary adapters stay.
        assert!(registry.get("echo").is_some());
    }

    #[test]
    fn a_mock_node_has_the_fixtures() {
        let registry = registry_for_node(true);
        assert!(registry.get(SECRET_ECHO_ADAPTER_ID).is_some());
        assert!(registry.get(SECRET_FORMS_ADAPTER_ID).is_some());
        assert!(registry.get(BULK_OUTPUT_ADAPTER_ID).is_some());
        assert!(registry.get("echo").is_some());
    }

    #[test]
    fn every_form_differs_from_raw_for_a_value_with_special_characters() {
        let value = "tok-ab12/+=&\"cd34";
        let text = forms_text(value, ALL_FORMS);
        for form in [
            "percent=",
            "json=",
            "base64=",
            "base64url=",
            "hex=",
            "basic=",
            "skew=",
        ] {
            let line = text.lines().find(|l| l.starts_with(form)).expect(form);
            assert!(!line.contains(value), "{line}");
        }
        assert!(text.contains("percent=tok-ab12%2F%2B%3D%26%22cd34"));
        assert!(text.contains("json=tok-ab12/+=&\\\"cd34"));
        assert!(text.contains(&format!("base64={}", STANDARD.encode(value))));
        assert!(text.contains("split=tok-ab12\n/"));
    }

    #[test]
    fn an_unknown_form_is_named() {
        assert_eq!(forms_text("x", &["nope"]), "nope=(unknown form nope)");
    }

    #[test]
    fn bulk_output_returns_the_requested_size() {
        let adapter = BulkOutputAdapter;
        let args = prost_types::Struct::default();
        let mut config = HashMap::new();
        config.insert("bytes".to_string(), "200000".to_string());
        let ctx = HostToolContext {
            session_id: "s",
            tool_name: "t",
            args: &args,
            config: &config,
            required_secrets: &[],
            vault: None,
            caller_user_id: "",
            timeout_ms: 1000,
            idempotency_key: None,
        };
        let out = adapter.invoke(&ctx).unwrap();
        assert_eq!(out.text.len(), 200_000);
        let mut bad = HashMap::new();
        bad.insert("bytes".to_string(), "many".to_string());
        let ctx = HostToolContext {
            config: &bad,
            ..ctx
        };
        assert!(adapter.invoke(&ctx).is_err());
    }
}
