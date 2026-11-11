use secrecy::ExposeSecret;
use std::collections::HashMap;
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

use crate::host_tools::{HostToolAdapter, HostToolContext, HostToolOutput};
use crate::ssrf_guard::PinnedUrl;
use prost_types::Struct;

/// Helper to get a string field from a prost Struct.
fn get_string_field(s: &Struct, key: &str) -> Option<String> {
    s.fields.get(key).and_then(|v| {
        if let Some(prost_types::value::Kind::StringValue(ref sv)) = v.kind {
            Some(sv.clone())
        } else {
            None
        }
    })
}

/// Helper to get an integer field from a prost Struct.
fn get_int_field(s: &Struct, key: &str) -> Option<i64> {
    s.fields.get(key).and_then(|v| {
        if let Some(prost_types::value::Kind::NumberValue(ref nv)) = v.kind {
            Some(*nv as i64)
        } else {
            None
        }
    })
}

/// Helper to get header map from a prost Struct.
fn get_headers_field(s: &Struct, key: &str) -> Option<HashMap<String, String>> {
    s.fields.get(key).and_then(|v| {
        if let Some(prost_types::value::Kind::StructValue(ref sv)) = v.kind {
            let mut headers = HashMap::new();
            for (hk, hv) in &sv.fields {
                if let Some(prost_types::value::Kind::StringValue(ref hvv)) = hv.kind {
                    headers.insert(hk.clone(), hvv.clone());
                }
            }
            Some(headers)
        } else {
            None
        }
    })
}

/// Core HTTP client invocation executed inside a dedicated OS thread.
#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
fn execute_http_request(
    method: &str,
    target_url: &str,
    headers: HashMap<String, String>,
    body_str: Option<String>,
    json_val: Option<serde_json::Value>,
    allowlist: Vec<String>,
    auth_header_val: Option<String>,
    timeout_ms: u64,
) -> Result<HostToolOutput, String> {
    // 1. SSRF validate and pin URL
    let pinned = PinnedUrl::validate_and_pin(target_url, &allowlist)?;

    // 2. Build reqwest blocking client
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .connect_timeout(Duration::from_millis(5000))
        .redirect(reqwest::redirect::Policy::none()); // Do not follow redirects by default to prevent SSRF bypass

    builder = pinned.apply_to_client_builder(builder);

    let client = builder
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    // 3. Construct request
    let mut req_builder = match method.to_uppercase().as_str() {
        "GET" => client.get(pinned.final_url.as_str()),
        "HEAD" => client.head(pinned.final_url.as_str()),
        "POST" => client.post(pinned.final_url.as_str()),
        "PUT" => client.put(pinned.final_url.as_str()),
        "PATCH" => client.patch(pinned.final_url.as_str()),
        "DELETE" => client.delete(pinned.final_url.as_str()),
        _ => return Err(format!("Unsupported HTTP method: {}", method)),
    };

    // Apply custom headers
    for (k, v) in headers {
        req_builder = req_builder.header(&k, &v);
    }

    // Apply authorization if retrieved from secrets
    if let Some(auth_val) = auth_header_val {
        req_builder = req_builder.header("Authorization", auth_val);
    }

    // Apply body or JSON
    if let Some(ref b) = body_str {
        req_builder = req_builder.body(b.clone());
    } else if let Some(ref j) = json_val {
        req_builder = req_builder.json(j);
    }

    // 4. Send request
    let response = req_builder
        .send()
        .map_err(|e| format!("HTTP request failed: {}", e))?;

    let status = response.status();
    let text = response
        .text()
        .map_err(|e| format!("Failed to read HTTP response body: {}", e))?;

    // Format final response info
    let output_text = format!(
        "HTTP/1.1 {} {}\n\n{}",
        status.as_u16(),
        status.canonical_reason().unwrap_or(""),
        text
    );

    Ok(HostToolOutput {
        text: output_text,
        artifact: None,
    })
}

/// Helper to execute the thread-bounded request with a timeout backstop.
fn invoke_with_thread_boundary(
    _method: &str,
    ctx: &HostToolContext<'_>,
    default_method: &str,
) -> Result<HostToolOutput, String> {
    let raw_url = get_string_field(ctx.args, "url")
        .ok_or_else(|| "Missing required argument 'url'".to_string())?;

    // Resolve base-url from secrets if URL is relative
    let mut target_url = raw_url.clone();
    let mut base_url_val = None;
    let mut auth_header_val = None;

    // The pinned destinations of each secret that will be sent, checked against the host the
    // request goes to once the URL is final.
    let mut base_url_pins: Option<(String, Vec<String>)> = None;
    let mut auth_header_pins: Option<(String, Vec<String>)> = None;

    if let Some(vault) = ctx.vault {
        for requirement in ctx.required_secrets {
            let secret_name = requirement.name.as_str();
            let is_base = secret_name.ends_with("base-url");
            let is_auth = secret_name.ends_with("auth-header");
            if !is_base && !is_auth {
                continue;
            }
            // A missing secret is skipped, as before. An MCP connection credential is refused
            // outright: a skill must not be able to name one.
            let (value, pins) =
                match vault.decrypt_secret_for_host_call(ctx.caller_user_id, secret_name) {
                    Ok(found) => found,
                    Err(crate::error::OrchestratorError::PermissionDenied(message)) => {
                        return Err(format!("secret_not_permitted: {message}"));
                    }
                    Err(_) => continue,
                };
            if is_base {
                base_url_val = Some(value.expose_secret().clone());
                base_url_pins = Some((secret_name.to_string(), pins));
            } else {
                auth_header_val = Some(value.expose_secret().clone());
                auth_header_pins = Some((secret_name.to_string(), pins));
            }
        }
    }

    if target_url.starts_with('/') {
        if let Some(ref base) = base_url_val {
            let base_trimmed = base.trim_end_matches('/');
            target_url = format!("{}{}", base_trimmed, target_url);
        } else {
            return Err("URL is relative but no 'base-url' secret is provided".to_string());
        }
    } else {
        // An absolute URL does not use the base URL, so it is not sent anywhere.
        base_url_pins = None;
    }

    // A secret goes only to a host it is pinned to. The Authorization value is sent to
    // whatever host the (skill-configured) allowlist lets the request reach, so the row's own
    // destinations decide. An empty list pins nothing, so it allows nothing. A base URL with
    // no destinations is pinned to the host its own value names.
    if base_url_pins.is_some() || auth_header_pins.is_some() {
        let host = url::Url::parse(&target_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .ok_or_else(|| "URL has no host".to_string())?;
        if let Some((name, mut pins)) = base_url_pins {
            if pins.is_empty() {
                pins = base_url_val
                    .as_deref()
                    .and_then(crate::secrets::host_from_url_secret)
                    .into_iter()
                    .collect();
            }
            if !crate::secrets::destination_allows_host(&pins, &host) {
                return Err(format!(
                    "secret_not_permitted: secret '{name}' is not pinned to host '{host}'"
                ));
            }
        }
        if let Some((name, pins)) = auth_header_pins {
            if !crate::secrets::destination_allows_host(&pins, &host) {
                return Err(format!(
                    "secret_not_permitted: secret '{name}' is not pinned to host '{host}'; \
                     pin it with --allowed-destination"
                ));
            }
        }
    }

    let mut headers = get_headers_field(ctx.args, "headers").unwrap_or_default();
    // The journal's key goes to the API only when the manifest says the API honours one.
    if let (Some(name), Some(key)) = (
        ctx.config
            .get("idempotency_header")
            .filter(|n| !n.trim().is_empty()),
        ctx.idempotency_key,
    ) {
        headers.insert(name.trim().to_string(), key.to_string());
    }
    let body_str = get_string_field(ctx.args, "body");

    // Extract JSON if present as a Struct
    let json_val = ctx.args.fields.get("json").and_then(|v| {
        if let Some(prost_types::value::Kind::StructValue(ref sv)) = v.kind {
            // Convert prost struct to serde_json
            Some(prost_struct_to_json(sv))
        } else {
            None
        }
    });

    let config_allowlist = ctx.config.get("allowlist").cloned().unwrap_or_default();
    let allowlist: Vec<String> = config_allowlist
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let method_to_use =
        get_string_field(ctx.args, "method").unwrap_or_else(|| default_method.to_string());

    let timeout_ms = get_int_field(ctx.args, "timeout_ms")
        .map(|v| v as u64)
        .unwrap_or(ctx.timeout_ms);

    // Run on a separate thread to avoid blocking the Tokio runtime
    let (tx, rx) = channel();
    let method_thread = method_to_use.clone();

    thread::spawn(move || {
        let res = execute_http_request(
            &method_thread,
            &target_url,
            headers,
            body_str,
            json_val,
            allowlist,
            auth_header_val,
            timeout_ms,
        );
        let _ = tx.send(res);
    });

    // Add extra padding to timeout to let the thread terminate or fail first
    let select_timeout = Duration::from_millis(timeout_ms + 1000);
    match rx.recv_timeout(select_timeout) {
        Ok(res) => res,
        Err(_) => Err("SSRF/HTTP Execution Thread Timed Out".to_string()),
    }
}

/// Helper to convert prost Struct to serde_json::Value
fn prost_struct_to_json(s: &Struct) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (k, v) in &s.fields {
        if let Some(ref kind) = v.kind {
            let val = match kind {
                prost_types::value::Kind::NullValue(_) => serde_json::Value::Null,
                prost_types::value::Kind::NumberValue(n) => {
                    if let Some(num) = serde_json::Number::from_f64(*n) {
                        serde_json::Value::Number(num)
                    } else {
                        serde_json::Value::Null
                    }
                }
                prost_types::value::Kind::StringValue(s) => serde_json::Value::String(s.clone()),
                prost_types::value::Kind::BoolValue(b) => serde_json::Value::Bool(*b),
                prost_types::value::Kind::StructValue(st) => prost_struct_to_json(st),
                prost_types::value::Kind::ListValue(lv) => {
                    let items: Vec<serde_json::Value> = lv
                        .values
                        .iter()
                        .map(|item| {
                            if let Some(ref k) = item.kind {
                                match k {
                                    prost_types::value::Kind::NullValue(_) => {
                                        serde_json::Value::Null
                                    }
                                    prost_types::value::Kind::NumberValue(n) => {
                                        if let Some(num) = serde_json::Number::from_f64(*n) {
                                            serde_json::Value::Number(num)
                                        } else {
                                            serde_json::Value::Null
                                        }
                                    }
                                    prost_types::value::Kind::StringValue(s) => {
                                        serde_json::Value::String(s.clone())
                                    }
                                    prost_types::value::Kind::BoolValue(b) => {
                                        serde_json::Value::Bool(*b)
                                    }
                                    prost_types::value::Kind::StructValue(st) => {
                                        prost_struct_to_json(st)
                                    }
                                    _ => serde_json::Value::Null,
                                }
                            } else {
                                serde_json::Value::Null
                            }
                        })
                        .collect();
                    serde_json::Value::Array(items)
                }
            };
            map.insert(k.clone(), val);
        }
    }
    serde_json::Value::Object(map)
}

/// Read-Only HTTP GET/HEAD Adapter
pub struct HttpGetAdapter;

impl HostToolAdapter for HttpGetAdapter {
    fn adapter_id(&self) -> &str {
        "http_get"
    }

    fn invoke(&self, ctx: &HostToolContext<'_>) -> Result<HostToolOutput, String> {
        invoke_with_thread_boundary("GET", ctx, "GET")
    }

    fn failure_is_definite(&self, error: &str) -> bool {
        http_failure_is_definite(error)
    }
}

/// Validation, URL admission and missing secrets fail before a request exists; a failed or timed
/// out send does not prove the request never arrived.
fn http_failure_is_definite(error: &str) -> bool {
    !(error.starts_with("HTTP request failed")
        || error.contains("Timed Out")
        || error.starts_with("Failed to read HTTP response body"))
}

/// Mutable HTTP POST/PUT/PATCH/DELETE Adapter
pub struct HttpWriteAdapter;

impl HostToolAdapter for HttpWriteAdapter {
    fn adapter_id(&self) -> &str {
        "http_write"
    }

    fn invoke(&self, ctx: &HostToolContext<'_>) -> Result<HostToolOutput, String> {
        invoke_with_thread_boundary("POST", ctx, "POST")
    }

    fn failure_is_definite(&self, error: &str) -> bool {
        http_failure_is_definite(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_tools::HostToolContext;
    use prost_types::{value::Kind, Value};
    use std::collections::BTreeMap;

    #[test]
    fn test_prost_struct_to_json() {
        let mut fields = BTreeMap::new();
        fields.insert(
            "foo".to_string(),
            Value {
                kind: Some(Kind::StringValue("bar".to_string())),
            },
        );
        fields.insert(
            "num".to_string(),
            Value {
                kind: Some(Kind::NumberValue(42.0)),
            },
        );
        let prost_struct = Struct { fields };
        let json = prost_struct_to_json(&prost_struct);
        assert_eq!(json["foo"], "bar");
        assert_eq!(json["num"].as_f64(), Some(42.0));
    }

    #[test]
    fn test_http_get_missing_url() {
        let ctx = HostToolContext {
            session_id: "test",
            tool_name: "http_get",
            args: &Struct::default(),
            config: &HashMap::new(),
            required_secrets: &[],
            vault: None,
            caller_user_id: "",
            timeout_ms: 1000,
            idempotency_key: None,
        };

        let adapter = HttpGetAdapter;
        let res = adapter.invoke(&ctx);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err(), "Missing required argument 'url'");
    }

    #[test]
    fn test_relative_url_without_base_url() {
        let mut fields = BTreeMap::new();
        fields.insert(
            "url".to_string(),
            Value {
                kind: Some(Kind::StringValue("/relative-path".to_string())),
            },
        );
        let args = Struct { fields };

        let ctx = HostToolContext {
            session_id: "test",
            tool_name: "http_get",
            args: &args,
            config: &HashMap::new(),
            required_secrets: &[],
            vault: None,
            caller_user_id: "",
            timeout_ms: 1000,
            idempotency_key: None,
        };

        let adapter = HttpGetAdapter;
        let res = adapter.invoke(&ctx);
        assert!(res.is_err());
        assert_eq!(
            res.unwrap_err(),
            "URL is relative but no 'base-url' secret is provided"
        );
    }

    // ---- secrets are sent only to hosts they are pinned to ---------------------------------

    use crate::secrets::{SecretKind, SecretsVault};
    use crate::workflow::SqliteWorkflowStore;
    use std::sync::Arc;

    fn vault() -> Arc<SecretsVault> {
        SecretsVault::for_tests(Arc::new(SqliteWorkflowStore::in_memory().unwrap()))
    }

    fn requirement(name: &str) -> vam_spec::v1::SecretRequirement {
        vam_spec::v1::SecretRequirement {
            name: name.to_string(),
            ..Default::default()
        }
    }

    /// Runs `http_get` for `url` with the named secrets required. The config has no allowlist,
    /// so a call that gets past the secret checks stops at URL admission with no network use.
    fn get(vault: &SecretsVault, url: &str, secrets: &[&str]) -> Result<HostToolOutput, String> {
        let mut fields = BTreeMap::new();
        fields.insert(
            "url".to_string(),
            Value {
                kind: Some(Kind::StringValue(url.to_string())),
            },
        );
        let args = Struct { fields };
        let required: Vec<_> = secrets.iter().map(|n| requirement(n)).collect();
        let ctx = HostToolContext {
            session_id: "test",
            tool_name: "http_get",
            args: &args,
            config: &HashMap::new(),
            required_secrets: &required,
            vault: Some(vault),
            caller_user_id: "",
            timeout_ms: 1000,
            idempotency_key: None,
        };
        HttpGetAdapter.invoke(&ctx)
    }

    fn refused(r: Result<HostToolOutput, String>) -> String {
        let err = r.expect_err("the call must be refused");
        assert!(err.starts_with("secret_not_permitted"), "{err}");
        err
    }

    fn passes_the_secret_checks(r: Result<HostToolOutput, String>) {
        match r {
            Ok(_) => {}
            Err(err) => assert!(!err.starts_with("secret_not_permitted"), "{err}"),
        }
    }

    #[test]
    fn a_skill_cannot_name_an_mcp_connection_credential() {
        let v = vault();
        v.put_secret(
            "acme/auth-header",
            "Bearer MCP-STATIC-TOKEN",
            SecretKind::Connection,
            &["mcp.example.com".to_string()],
            None,
        )
        .unwrap();
        // Even to the host the credential is pinned to.
        let err = refused(get(
            &v,
            "https://mcp.example.com/mcp",
            &["acme/auth-header"],
        ));
        assert!(!err.contains("MCP-STATIC-TOKEN"), "{err}");
        assert!(err.contains("MCP connection"), "{err}");
    }

    #[test]
    fn the_authorization_value_goes_only_to_a_host_the_row_is_pinned_to() {
        let v = vault();
        let pin = |name: &str, dests: &[&str]| {
            v.put_secret(
                name,
                "Bearer token",
                SecretKind::Egress,
                &dests.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
                None,
            )
            .unwrap();
        };
        pin("jira/auth-header", &["*.atlassian.net"]);
        pin("exact/auth-header", &["api.example.com"]);
        pin("open/auth-header", &["*"]);
        pin("unpinned/auth-header", &[]);

        // Inside the pin.
        passes_the_secret_checks(get(
            &v,
            "https://acme.atlassian.net/rest",
            &["jira/auth-header"],
        ));
        passes_the_secret_checks(get(&v, "https://api.example.com/x", &["exact/auth-header"]));
        passes_the_secret_checks(get(&v, "https://anywhere.example/x", &["open/auth-header"]));

        // Outside it: another host, the apex of a wildcard, a lookalike suffix, a subdomain of
        // an exact pin.
        let err = refused(get(&v, "https://evil.example/x", &["jira/auth-header"]));
        assert!(
            err.contains("evil.example") && !err.contains("Bearer"),
            "{err}"
        );
        refused(get(&v, "https://atlassian.net/x", &["jira/auth-header"]));
        refused(get(
            &v,
            "https://acme.atlassian.net.evil.example/x",
            &["jira/auth-header"],
        ));
        refused(get(
            &v,
            "https://evil.api.example.com/x",
            &["exact/auth-header"],
        ));
        // No pinned destination allows nothing.
        refused(get(
            &v,
            "https://api.example.com/x",
            &["unpinned/auth-header"],
        ));
    }

    #[test]
    fn a_base_url_is_pinned_to_its_own_host_unless_the_row_says_otherwise() {
        let v = vault();
        v.put_secret(
            "hrms/base-url",
            "https://hrms.example.com",
            SecretKind::Egress,
            &[],
            None,
        )
        .unwrap();
        v.put_secret(
            "hrms/auth-header",
            "Bearer token",
            SecretKind::Egress,
            &["hrms.example.com".to_string()],
            None,
        )
        .unwrap();
        // Relative URL: the base URL supplies the host, and both rows agree with it.
        passes_the_secret_checks(get(&v, "/people", &["hrms/base-url", "hrms/auth-header"]));

        // The base URL row is pinned to another host than its value names: refused.
        v.put_secret(
            "odd/base-url",
            "https://hrms.example.com",
            SecretKind::Egress,
            &["other.example.com".to_string()],
            None,
        )
        .unwrap();
        refused(get(&v, "/people", &["odd/base-url"]));

        // An absolute URL does not use the base URL, so only the auth row is judged.
        passes_the_secret_checks(get(
            &v,
            "https://hrms.example.com/people",
            &["odd/base-url", "hrms/auth-header"],
        ));
    }

    #[test]
    fn a_connection_row_named_as_a_base_url_is_refused_too() {
        let v = vault();
        v.put_secret(
            "acme/base-url",
            "https://mcp.example.com",
            SecretKind::Connection,
            &["mcp.example.com".to_string()],
            None,
        )
        .unwrap();
        refused(get(&v, "/mcp", &["acme/base-url"]));
    }

    #[test]
    fn a_missing_secret_is_still_skipped() {
        let v = vault();
        let err = get(&v, "/x", &["nothing/base-url"]).unwrap_err();
        assert_eq!(err, "URL is relative but no 'base-url' secret is provided");
    }
}
