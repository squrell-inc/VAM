use crate::error::VamNetworkError;
use secrecy::{ExposeSecret, SecretString};
use sha2::Digest;
use std::collections::HashMap;
use std::sync::Mutex;
use vam_spec::v1::{EgressRequest, EgressResponse, SecretBinding};
use zeroize::Zeroize;

pub trait SecretProvider: Send + Sync + std::fmt::Debug {
    fn get_secret(&self, secret_ref: &str) -> Result<SecretString, VamNetworkError>;
}

#[derive(Debug)]
pub struct InMemorySecretProvider {
    secrets: Mutex<HashMap<String, SecretString>>,
}

impl Default for InMemorySecretProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemorySecretProvider {
    pub fn new() -> Self {
        Self {
            secrets: Mutex::new(HashMap::new()),
        }
    }

    pub fn insert(&self, key: &str, secret: &str) {
        self.secrets
            .lock()
            .unwrap()
            .insert(key.to_string(), SecretString::new(secret.to_string()));
    }
}

impl SecretProvider for InMemorySecretProvider {
    fn get_secret(&self, secret_ref: &str) -> Result<SecretString, VamNetworkError> {
        self.secrets
            .lock()
            .unwrap()
            .get(secret_ref)
            .cloned()
            .ok_or_else(|| VamNetworkError::SecretNotFound {
                key: secret_ref.to_string(),
            })
    }
}

/// Run-stable guest placeholder: `vamsec_` plus 64 hex characters.
///
/// The token is HMAC-SHA256 (RFC 2104) keyed by the raw host secret. The
/// message is a domain-separated, length-prefixed encoding of `run_id` and
/// `binding_id`, so those fields cannot be transposed and neither is copied
/// into the token. Session identity is not an input: a later VM for the same
/// run mints the same placeholder, and history that already contains it still
/// substitutes. The result never contains the secret, the binding id, or a
/// host path.
pub fn generate_placeholder(host_secret: &str, run_id: &str, binding_id: &str) -> String {
    let message = placeholder_message(run_id, binding_id);
    let mut mac = hmac_sha256(host_secret.as_bytes(), &message);
    let mut placeholder = String::from("vamsec_");
    placeholder.push_str(&hex_encode(&mac));
    mac.zeroize();
    placeholder
}

fn placeholder_message(run_id: &str, binding_id: &str) -> Vec<u8> {
    let mut message = Vec::with_capacity(32 + run_id.len() + binding_id.len());
    message.extend_from_slice(b"vam.placeholder.v1");
    push_labeled_field(&mut message, b"run", run_id.as_bytes());
    push_labeled_field(&mut message, b"binding", binding_id.as_bytes());
    message
}

fn push_labeled_field(message: &mut Vec<u8>, label: &[u8], value: &[u8]) {
    message.extend_from_slice(label);
    message.extend_from_slice(&(value.len() as u64).to_be_bytes());
    message.extend_from_slice(value);
}

/// HMAC-SHA256. Key copies are zeroized before return. Accepts any key length.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = sha2::Sha256::digest(key);
        let mut hashed = [0u8; 32];
        hashed.copy_from_slice(&digest);
        key_block[..hashed.len()].copy_from_slice(&hashed);
        hashed.zeroize();
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0u8; BLOCK];
    let mut opad = [0u8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] = key_block[i] ^ 0x36;
        opad[i] = key_block[i] ^ 0x5c;
    }
    key_block.zeroize();

    let mut inner = sha2::Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner_digest = inner.finalize();
    ipad.zeroize();

    let mut outer = sha2::Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    let outer_digest = outer.finalize();
    opad.zeroize();

    let mut out = [0u8; 32];
    out.copy_from_slice(&outer_digest);
    out
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Tracks which bindings had URL-site placeholders substituted during phase 1.
#[derive(Debug, Default)]
pub struct SecretInjectionPlan {
    pub url_substituted: Vec<usize>,
}

/// Normalizes a URL after placeholder substitution:
/// - Collapses duplicated schemes (e.g. `https://https://` -> `https://`)
/// - Prepends `https://` when no scheme is present
pub fn normalize_effective_url(url: &str) -> String {
    let mut result = url.trim().to_string();
    result = result
        .replace("https://https://", "https://")
        .replace("http://http://", "http://")
        .replace("https://http://", "https://");

    if !result.starts_with("http://") && !result.starts_with("https://") {
        result = format!("https://{}", result);
    }
    result
}

fn placeholder_in_body(request: &EgressRequest, placeholder: &str) -> bool {
    if let Ok(body_str) = std::str::from_utf8(&request.body) {
        body_str.contains(placeholder)
    } else {
        let p_bytes = placeholder.as_bytes();
        request
            .body
            .windows(p_bytes.len())
            .any(|window| window == p_bytes)
    }
}

fn scan_header_occurrences(request: &EgressRequest, placeholder: &str) -> Vec<String> {
    request
        .headers
        .iter()
        .filter_map(|(k, v)| {
            if v.contains(placeholder) {
                Some(k.clone())
            } else {
                None
            }
        })
        .collect()
}

fn is_destination_allowed(binding: &SecretBinding, destination_host: &str) -> bool {
    if binding.allowed_destinations.is_empty() {
        return false;
    }
    binding
        .allowed_destinations
        .iter()
        .any(|pattern| pattern == "*" || crate::policy::match_domain(pattern, destination_host))
}

/// True when `host` is a destination of any binding that has a placeholder, whether or not a
/// given request carries it. The proxy uses this to refuse a response it cannot scrub: an
/// upstream can echo a secret it was sent earlier. A destination pattern of `*` makes every host
/// bound, which is what that pattern means for injection too.
pub fn destination_holds_secret(bindings: &[SecretBinding], host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    bindings
        .iter()
        .any(|binding| !binding.placeholder.is_empty() && is_destination_allowed(binding, &host))
}

/// Replaces the placeholder in every header whose name matches `declared`.
/// Header names are case-insensitive (RFC 9110 section 5.1) and hyper hands
/// them over lowercased, so the match ignores case. Returns false when the
/// request has no such header.
fn replace_in_declared_header(
    request: &mut EgressRequest,
    declared: &str,
    placeholder: &str,
    secret_val: &str,
) -> bool {
    let mut found = false;
    for (name, value) in request.headers.iter_mut() {
        if name.eq_ignore_ascii_case(declared) {
            found = true;
            *value = value.replace(placeholder, secret_val);
        }
    }
    found
}

/// Refuses a request that would carry a real secret in cleartext. Called once
/// the effective URL is known, before the destination check. A URL-site
/// substitution from phase 1 counts, because the plan records it.
pub fn refuse_placeholders_over_plain_http(
    request: &EgressRequest,
    bindings: &[SecretBinding],
    plan: &SecretInjectionPlan,
    scheme: &str,
) -> Result<(), VamNetworkError> {
    if !scheme.eq_ignore_ascii_case("http") {
        return Ok(());
    }
    let carries_secret = !plan.url_substituted.is_empty()
        || bindings.iter().any(|binding| {
            !binding.placeholder.is_empty()
                && (!scan_header_occurrences(request, &binding.placeholder).is_empty()
                    || placeholder_in_body(request, &binding.placeholder))
        });
    if carries_secret {
        return Err(VamNetworkError::EgressForbidden {
            reason: "Secret placeholder refused over plain HTTP: secrets are only sent over HTTPS"
                .to_string(),
        });
    }
    Ok(())
}

/// Phase 1: substitute URL-site (QUERY_PARAM) placeholders and normalize the effective URL.
/// Does not perform destination validation or header/body injection.
pub fn phase1_substitute_url_secrets(
    request: &mut EgressRequest,
    bindings: &[SecretBinding],
    provider: &dyn SecretProvider,
) -> Result<SecretInjectionPlan, VamNetworkError> {
    let mut plan = SecretInjectionPlan::default();

    for (idx, binding) in bindings.iter().enumerate() {
        let placeholder = &binding.placeholder;
        if placeholder.is_empty() {
            continue;
        }

        let site = binding
            .inject_at
            .as_ref()
            .ok_or_else(|| VamNetworkError::EgressForbidden {
                reason: format!(
                    "Secret binding '{}' has no declared injection site",
                    binding.binding_id
                ),
            })?;

        if site.r#type != vam_spec::v1::InjectionSiteType::InjectionSiteQueryParam as i32 {
            continue;
        }

        if !request.url.contains(placeholder) {
            continue;
        }

        let in_headers = scan_header_occurrences(request, placeholder);
        if placeholder_in_body(request, placeholder) || !in_headers.is_empty() {
            return Err(VamNetworkError::EgressForbidden {
                reason: format!(
                    "Security boundary violation: placeholder '{}' found outside declared QueryParam site",
                    placeholder
                ),
            });
        }

        let secret = provider.get_secret(&binding.secret_ref)?;
        let secret_val = secret.expose_secret().trim();
        request.url = request.url.replace(placeholder, secret_val);
        plan.url_substituted.push(idx);
    }

    request.url = normalize_effective_url(&request.url);
    Ok(plan)
}

/// Phase 2: validate allowed_destinations against the real host, then inject header/body secrets.
pub fn phase2_inject_secrets(
    request: &mut EgressRequest,
    bindings: &[SecretBinding],
    provider: &dyn SecretProvider,
    destination_host: &str,
    plan: &SecretInjectionPlan,
) -> Result<(), VamNetworkError> {
    for (idx, binding) in bindings.iter().enumerate() {
        let placeholder = &binding.placeholder;
        if placeholder.is_empty() {
            continue;
        }

        let in_url = plan.url_substituted.contains(&idx);
        let in_headers = scan_header_occurrences(request, placeholder);
        let in_body = placeholder_in_body(request, placeholder);

        let has_occurrence = in_url || !in_headers.is_empty() || in_body;
        if !has_occurrence {
            continue;
        }

        if !is_destination_allowed(binding, destination_host) {
            return Err(VamNetworkError::EgressForbidden {
                reason: format!(
                    "Confused-deputy blocked: placeholder '{}' cannot be transmitted to host '{}'",
                    placeholder, destination_host
                ),
            });
        }

        let site = binding
            .inject_at
            .as_ref()
            .ok_or_else(|| VamNetworkError::EgressForbidden {
                reason: format!(
                    "Secret binding '{}' has no declared injection site",
                    binding.binding_id
                ),
            })?;

        let secret = provider.get_secret(&binding.secret_ref)?;
        let secret_val = secret.expose_secret();

        match vam_spec::v1::InjectionSiteType::try_from(site.r#type).map_err(|_| {
            VamNetworkError::EgressForbidden {
                reason: "Invalid InjectionSiteType".to_string(),
            }
        })? {
            vam_spec::v1::InjectionSiteType::InjectionSiteQueryParam => {
                // URL substitution completed in phase 1.
                continue;
            }
            vam_spec::v1::InjectionSiteType::InjectionSiteHeader => {
                if in_url
                    || in_body
                    || in_headers
                        .iter()
                        .any(|h| !h.eq_ignore_ascii_case(&site.key))
                {
                    return Err(VamNetworkError::EgressForbidden {
                        reason: format!(
                            "Security boundary violation: placeholder '{}' found outside declared Header site",
                            placeholder
                        ),
                    });
                }
                if !replace_in_declared_header(request, &site.key, placeholder, secret_val) {
                    return Err(VamNetworkError::EgressForbidden {
                        reason: format!("Declared Header '{}' not found in request", site.key),
                    });
                }
            }
            vam_spec::v1::InjectionSiteType::InjectionSiteBearerToken => {
                if in_url
                    || in_body
                    || in_headers
                        .iter()
                        .any(|h| !h.eq_ignore_ascii_case("Authorization"))
                {
                    return Err(VamNetworkError::EgressForbidden {
                        reason: format!(
                            "Security boundary violation: placeholder '{}' found outside declared BearerToken site",
                            placeholder
                        ),
                    });
                }
                if !replace_in_declared_header(request, "Authorization", placeholder, secret_val) {
                    return Err(VamNetworkError::EgressForbidden {
                        reason: "Authorization header not found in request".to_string(),
                    });
                }
            }
            vam_spec::v1::InjectionSiteType::InjectionSiteBodyJsonPointer => {
                if in_url || !in_headers.is_empty() {
                    return Err(VamNetworkError::EgressForbidden {
                        reason: format!(
                            "Security boundary violation: placeholder '{}' found outside declared BodyJsonPointer site",
                            placeholder
                        ),
                    });
                }
                let body_str = std::str::from_utf8(&request.body).map_err(|_| {
                    VamNetworkError::EgressForbidden {
                        reason: "Request body is not valid UTF-8 for JSON injection".to_string(),
                    }
                })?;
                let replaced = body_str.replace(placeholder, secret_val);
                request.body = replaced.into_bytes();
            }
            _ => {
                return Err(VamNetworkError::EgressForbidden {
                    reason: "Unsupported injection site type".to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Convenience wrapper that runs both injection phases against a known destination host.
pub fn inject_secrets(
    request: &mut EgressRequest,
    bindings: &[SecretBinding],
    provider: &dyn SecretProvider,
    destination_host: &str,
) -> Result<(), VamNetworkError> {
    let plan = phase1_substitute_url_secrets(request, bindings, provider)?;
    phase2_inject_secrets(request, bindings, provider, destination_host, &plan)
}

/// Scrubs a secret out of response bodies and headers to prevent leakages back to the guest:
/// the raw value, its encoded forms (base64, hex, percent, JSON-escaped), the value split
/// across whitespace and any run of 12 or more of its bytes (see
/// `vam_security::secret_forms`). Each run becomes the binding's placeholder.
pub fn scrub_response(
    response: &mut EgressResponse,
    bindings: &[SecretBinding],
    provider: &dyn SecretProvider,
) {
    for binding in bindings {
        if let Ok(secret) = provider.get_secret(&binding.secret_ref) {
            let secret_val = secret.expose_secret();
            if secret_val.is_empty() {
                continue;
            }
            let redactor = vam_security::secret_forms::SecretRedactor::with_replacement(
                &[secret_val.as_bytes()],
                &binding.placeholder,
            );

            // Scrub headers
            for val in response.headers.values_mut() {
                if let std::borrow::Cow::Owned(scrubbed) = redactor.redact_str(val) {
                    *val = scrubbed;
                }
            }

            // Scrub body, as bytes: a binary body keeps every byte outside the runs.
            if let std::borrow::Cow::Owned(scrubbed) = redactor.redact_bytes(&response.body) {
                response.body = scrubbed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::InjectionSite;

    #[test]
    fn destination_holds_secret_follows_the_binding_destinations() {
        let binding = |placeholder: &str, destinations: &[&str]| SecretBinding {
            binding_id: "k".to_string(),
            placeholder: placeholder.to_string(),
            secret_ref: "k".to_string(),
            allowed_destinations: destinations.iter().map(|d| d.to_string()).collect(),
            inject_at: None,
        };
        let bound = [binding(
            "vamsec_k",
            &["api.example.com", "*.cdn.example.com"],
        )];
        assert!(destination_holds_secret(&bound, "api.example.com"));
        assert!(destination_holds_secret(&bound, "API.Example.COM."));
        assert!(destination_holds_secret(&bound, "a.cdn.example.com"));
        assert!(!destination_holds_secret(&bound, "other.example.com"));
        assert!(!destination_holds_secret(&[], "api.example.com"));
        // No destinations means the secret is never sent anywhere; no placeholder means no binding.
        assert!(!destination_holds_secret(
            &[binding("vamsec_k", &[])],
            "api.example.com"
        ));
        assert!(!destination_holds_secret(
            &[binding("", &["api.example.com"])],
            "api.example.com"
        ));
        // `*` is every host, for injection and so for this.
        assert!(destination_holds_secret(
            &[binding("vamsec_k", &["*"])],
            "anywhere.test"
        ));
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_cases() {
        let short_key = [0x0bu8; 20];
        let short_mac = hmac_sha256(&short_key, b"Hi There");
        assert_eq!(
            hex_encode(&short_mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );

        // Case 4: key longer than the 64-byte block is hashed first.
        let long_key = [0xaau8; 131];
        let long_mac = hmac_sha256(
            &long_key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex_encode(&long_mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn placeholder_is_stable_for_a_run_and_hides_secret_material() {
        let host_secret = "sk_live_stripe_secret_key";
        let run_id = "urn:vam:run:local.vam.dev/acme/run-1";
        let binding_id = "stripe/api-key";

        let first_session = generate_placeholder(host_secret, run_id, binding_id);
        let resumed_session = generate_placeholder(host_secret, run_id, binding_id);
        assert_eq!(first_session, resumed_session);
        assert_eq!(
            first_session,
            "vamsec_e375721ad7a9afc7a0189af2544dabf67e5e7e376eb4869c24c422d0450a2832"
        );

        assert_ne!(
            first_session,
            generate_placeholder(
                host_secret,
                "urn:vam:run:local.vam.dev/acme/run-2",
                binding_id
            )
        );
        assert_ne!(
            first_session,
            generate_placeholder(host_secret, run_id, "other/api-key")
        );
        assert_ne!(
            first_session,
            generate_placeholder("sk_live_other", run_id, binding_id)
        );

        assert!(first_session.starts_with("vamsec_"));
        assert_eq!(first_session.len(), "vamsec_".len() + 64);
        assert!(!first_session.contains(host_secret));
        assert!(!first_session.contains("stripe"));
        assert!(!first_session.contains('/'));

        let path_binding = "/var/lib/vam/secrets.key";
        let path_secret = "/etc/vam/host-secret";
        let from_path = generate_placeholder(path_secret, run_id, path_binding);
        assert!(!from_path.contains("/var/lib"));
        assert!(!from_path.contains("/etc/vam"));
        assert!(!from_path.contains("secrets.key"));
        assert!(!from_path.contains("host-secret"));
        assert!(!from_path.contains('/'));
    }

    #[test]
    fn placeholder_stable_across_session_identity_and_substitutes_on_resume() {
        let host_secret = "sk_live_stripe_secret_key";
        let run_id = "urn:vam:run:local.vam.dev/acme/run-1";
        let binding_id = "stripe/api-key";
        let session_a = "sess-vm-1";
        let session_b = "sess-vm-2";

        let history_placeholder =
            placeholder_for_session(session_a, host_secret, run_id, binding_id);
        let resumed_placeholder =
            placeholder_for_session(session_b, host_secret, run_id, binding_id);
        assert_eq!(
            history_placeholder, resumed_placeholder,
            "a new VM for the same run must reproduce the history token"
        );
        assert_ne!(session_a, session_b);

        let provider = InMemorySecretProvider::new();
        provider.insert(binding_id, host_secret);
        let binding = SecretBinding {
            binding_id: binding_id.to_string(),
            placeholder: resumed_placeholder.clone(),
            secret_ref: binding_id.to_string(),
            allowed_destinations: vec!["api.stripe.com".to_string()],
            inject_at: Some(InjectionSite {
                r#type: vam_spec::v1::InjectionSiteType::InjectionSiteHeader as i32,
                key: "Authorization".to_string(),
            }),
        };

        let mut history_request = EgressRequest {
            method: "POST".to_string(),
            url: "https://api.stripe.com/v1/charges".to_string(),
            headers: vec![(
                "Authorization".to_string(),
                format!("Bearer {history_placeholder}"),
            )]
            .into_iter()
            .collect(),
            body: Vec::new(),
        };
        inject_secrets(
            &mut history_request,
            std::slice::from_ref(&binding),
            &provider,
            "api.stripe.com",
        )
        .unwrap();
        assert_eq!(
            history_request.headers.get("Authorization").unwrap(),
            "Bearer sk_live_stripe_secret_key"
        );

        let mut response = EgressResponse {
            status_code: 200,
            headers: vec![(
                "X-Request-Id".to_string(),
                "req-sk_live_stripe_secret_key".to_string(),
            )]
            .into_iter()
            .collect(),
            body: b"echo sk_live_stripe_secret_key".to_vec(),
            error: None,
        };
        scrub_response(&mut response, &[binding], &provider);
        let guest_header = response.headers.get("X-Request-Id").unwrap();
        let guest_body = std::str::from_utf8(&response.body).unwrap();
        assert_eq!(guest_header, &format!("req-{resumed_placeholder}"));
        assert_eq!(guest_body, format!("echo {resumed_placeholder}"));
        assert!(!guest_header.contains(host_secret));
        assert!(!guest_body.contains(host_secret));
        assert!(!guest_header.contains('/'));
    }

    /// Session identity is visible to the caller and must not change the token.
    fn placeholder_for_session(
        session_id: &str,
        host_secret: &str,
        run_id: &str,
        binding_id: &str,
    ) -> String {
        let placeholder = generate_placeholder(host_secret, run_id, binding_id);
        assert!(
            !placeholder.contains(session_id),
            "session identity must not appear in the placeholder"
        );
        placeholder
    }

    #[test]
    fn test_normalize_effective_url() {
        assert_eq!(
            normalize_effective_url("https://https://mysite.atlassian.net/rest/api/3/search"),
            "https://mysite.atlassian.net/rest/api/3/search"
        );
        assert_eq!(
            normalize_effective_url("mysite.atlassian.net/rest/api/3/search"),
            "https://mysite.atlassian.net/rest/api/3/search"
        );
        assert_eq!(
            normalize_effective_url("http://http://example.com/path"),
            "http://example.com/path"
        );
        assert_eq!(
            normalize_effective_url("https://example.com/path"),
            "https://example.com/path"
        );
    }

    #[test]
    fn test_secret_injection_and_scrub_workflow() {
        let provider = InMemorySecretProvider::new();
        provider.insert("prod/stripe", "sk_live_stripe_secret_key");

        let placeholder = generate_placeholder(
            "sk_live_stripe_secret_key",
            "urn:vam:run:test/stripe",
            "stripe",
        );
        let binding = SecretBinding {
            binding_id: "stripe".to_string(),
            placeholder: placeholder.clone(),
            secret_ref: "prod/stripe".to_string(),
            allowed_destinations: vec!["api.stripe.com".to_string()],
            inject_at: Some(InjectionSite {
                r#type: vam_spec::v1::InjectionSiteType::InjectionSiteHeader as i32,
                key: "Authorization".to_string(),
            }),
        };

        // 1. Valid injection
        let mut request = EgressRequest {
            method: "POST".to_string(),
            url: "https://api.stripe.com/v1/charges".to_string(),
            headers: vec![(
                "Authorization".to_string(),
                format!("Bearer {}", placeholder),
            )]
            .into_iter()
            .collect(),
            body: Vec::new(),
        };

        inject_secrets(
            &mut request,
            std::slice::from_ref(&binding),
            &provider,
            "api.stripe.com",
        )
        .unwrap();
        assert_eq!(
            request.headers.get("Authorization").unwrap(),
            "Bearer sk_live_stripe_secret_key"
        );

        // 2. Confused Deputy blocked
        let mut request_bad_dest = EgressRequest {
            method: "POST".to_string(),
            url: "https://attacker-domain.com/log".to_string(),
            headers: vec![(
                "Authorization".to_string(),
                format!("Bearer {}", placeholder),
            )]
            .into_iter()
            .collect(),
            body: Vec::new(),
        };
        let res = inject_secrets(
            &mut request_bad_dest,
            std::slice::from_ref(&binding),
            &provider,
            "attacker-domain.com",
        );
        assert!(res.is_err());

        // 3. Security Boundary violation (placeholder placed in wrong place, e.g. body instead of Header)
        let mut request_violation = EgressRequest {
            method: "POST".to_string(),
            url: "https://api.stripe.com/v1/charges".to_string(),
            headers: vec![(
                "Authorization".to_string(),
                format!("Bearer {}", placeholder),
            )]
            .into_iter()
            .collect(),
            body: placeholder.clone().into_bytes(), // Found in body too!
        };
        let res2 = inject_secrets(
            &mut request_violation,
            std::slice::from_ref(&binding),
            &provider,
            "api.stripe.com",
        );
        assert!(res2.is_err());

        // 4. Scrubbing responses
        let mut response = EgressResponse {
            status_code: 200,
            headers: vec![(
                "X-Response-Key".to_string(),
                "sk_live_stripe_secret_key".to_string(),
            )]
            .into_iter()
            .collect(),
            body: b"Your secret is sk_live_stripe_secret_key, secure.".to_vec(),
            error: None,
        };
        scrub_response(&mut response, &[binding], &provider);
        assert_eq!(
            response.headers.get("X-Response-Key").unwrap(),
            &placeholder
        );
        assert_eq!(
            std::str::from_utf8(&response.body).unwrap(),
            format!("Your secret is {}, secure.", placeholder)
        );
    }

    #[test]
    fn test_two_phase_url_substitution_then_destination_validation() {
        let provider = InMemorySecretProvider::new();
        provider.insert("jira/base-url", "https://mysite.atlassian.net");

        let url_binding = SecretBinding {
            binding_id: "jira_base_url".to_string(),
            placeholder: "vamsec_jira_base_url".to_string(),
            secret_ref: "jira/base-url".to_string(),
            allowed_destinations: vec!["*.atlassian.net".to_string()],
            inject_at: Some(InjectionSite {
                r#type: vam_spec::v1::InjectionSiteType::InjectionSiteQueryParam as i32,
                key: String::new(),
            }),
        };

        let mut request = EgressRequest {
            method: "GET".to_string(),
            url: "vamsec_jira_base_url/rest/api/3/search".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };

        let plan = phase1_substitute_url_secrets(
            &mut request,
            std::slice::from_ref(&url_binding),
            &provider,
        )
        .unwrap();
        assert_eq!(
            request.url,
            "https://mysite.atlassian.net/rest/api/3/search"
        );
        assert_eq!(plan.url_substituted, vec![0]);

        phase2_inject_secrets(
            &mut request,
            std::slice::from_ref(&url_binding),
            &provider,
            "mysite.atlassian.net",
            &plan,
        )
        .unwrap();

        let mut blocked = EgressRequest {
            method: "GET".to_string(),
            url: "vamsec_jira_base_url/rest/api/3/search".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let blocked_plan = phase1_substitute_url_secrets(
            &mut blocked,
            std::slice::from_ref(&url_binding),
            &provider,
        )
        .unwrap();
        let err = phase2_inject_secrets(
            &mut blocked,
            &[url_binding],
            &provider,
            "attacker.example.com",
            &blocked_plan,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Confused-deputy blocked"));
    }

    #[test]
    fn test_placeholder_in_non_declared_header_rejected() {
        let provider = InMemorySecretProvider::new();
        provider.insert("jira/base-url", "https://mysite.atlassian.net");

        let url_binding = SecretBinding {
            binding_id: "jira_base_url".to_string(),
            placeholder: "vamsec_jira_base_url".to_string(),
            secret_ref: "jira/base-url".to_string(),
            allowed_destinations: vec!["*.atlassian.net".to_string()],
            inject_at: Some(InjectionSite {
                r#type: vam_spec::v1::InjectionSiteType::InjectionSiteQueryParam as i32,
                key: String::new(),
            }),
        };

        let mut request = EgressRequest {
            method: "GET".to_string(),
            url: "https://127.0.0.1:8080/vamsec_jira_base_url/rest/api/3/search".to_string(),
            headers: vec![("Host".to_string(), "vamsec_jira_base_url".to_string())]
                .into_iter()
                .collect(),
            body: Vec::new(),
        };

        let err =
            phase1_substitute_url_secrets(&mut request, &[url_binding], &provider).unwrap_err();
        assert!(err.to_string().contains("outside declared QueryParam site"));
    }

    #[test]
    fn test_empty_allowed_destinations_fails_closed() {
        let provider = InMemorySecretProvider::new();
        provider.insert("prod/stripe", "sk_live_stripe_secret_key");

        let placeholder = generate_placeholder(
            "sk_live_stripe_secret_key",
            "urn:vam:run:test/stripe",
            "stripe",
        );
        let binding = SecretBinding {
            binding_id: "stripe".to_string(),
            placeholder: placeholder.clone(),
            secret_ref: "prod/stripe".to_string(),
            allowed_destinations: vec![],
            inject_at: Some(InjectionSite {
                r#type: vam_spec::v1::InjectionSiteType::InjectionSiteHeader as i32,
                key: "Authorization".to_string(),
            }),
        };

        let mut request = EgressRequest {
            method: "POST".to_string(),
            url: "https://api.stripe.com/v1/charges".to_string(),
            headers: vec![(
                "Authorization".to_string(),
                format!("Bearer {}", placeholder),
            )]
            .into_iter()
            .collect(),
            body: Vec::new(),
        };

        let plan =
            phase1_substitute_url_secrets(&mut request, std::slice::from_ref(&binding), &provider)
                .unwrap();
        let err =
            phase2_inject_secrets(&mut request, &[binding], &provider, "api.stripe.com", &plan)
                .unwrap_err();
        assert!(err.to_string().contains("Confused-deputy blocked"));
    }

    /// Hyper lowercases header names before they reach the gate, so the
    /// declared site key and the request name can differ only in case.
    #[test]
    fn declared_header_site_matches_lowercase_request_names() {
        let provider = InMemorySecretProvider::new();
        provider.insert("prod/api", "sk_live_real_value");
        let placeholder = generate_placeholder("sk_live_real_value", "urn:vam:run:test/api", "api");

        let cases = [
            (
                vam_spec::v1::InjectionSiteType::InjectionSiteHeader,
                "Authorization",
                "authorization",
                format!("Bearer {placeholder}"),
                "Bearer sk_live_real_value",
            ),
            (
                vam_spec::v1::InjectionSiteType::InjectionSiteHeader,
                "x-api-key",
                "X-Api-Key",
                placeholder.clone(),
                "sk_live_real_value",
            ),
            (
                vam_spec::v1::InjectionSiteType::InjectionSiteBearerToken,
                "Authorization",
                "authorization",
                format!("Bearer {placeholder}"),
                "Bearer sk_live_real_value",
            ),
        ];

        for (site_type, key, request_name, value, expected) in cases {
            let binding = SecretBinding {
                binding_id: "api".to_string(),
                placeholder: placeholder.clone(),
                secret_ref: "prod/api".to_string(),
                allowed_destinations: vec!["api.example.com".to_string()],
                inject_at: Some(InjectionSite {
                    r#type: site_type as i32,
                    key: key.to_string(),
                }),
            };
            let mut request = EgressRequest {
                method: "GET".to_string(),
                url: "https://api.example.com/v1/models".to_string(),
                headers: vec![(request_name.to_string(), value)]
                    .into_iter()
                    .collect(),
                body: Vec::new(),
            };

            inject_secrets(&mut request, &[binding], &provider, "api.example.com")
                .unwrap_or_else(|err| panic!("{key} / {request_name}: {err}"));
            assert_eq!(
                request.headers.get(request_name).unwrap(),
                expected,
                "{key} / {request_name}"
            );
        }
    }

    /// A vendor that echoes the credential encoded, split or in part gives the guest none of
    /// it. Values are generated; failures name the form, never the value.
    #[test]
    fn scrub_response_removes_encoded_split_and_partial_forms() {
        let value = format!("tok-{}/+=&\"z", "0f3a9c51d7e2b684a1c0d9e3");
        let provider = InMemorySecretProvider::new();
        provider.insert("prod/vendor", &value);
        let placeholder = "{{vam:vendor}}".to_string();
        let binding = SecretBinding {
            binding_id: "vendor".to_string(),
            placeholder: placeholder.clone(),
            secret_ref: "prod/vendor".to_string(),
            allowed_destinations: vec!["api.example.com".to_string()],
            inject_at: None,
        };
        let hex: String = value.bytes().map(|b| format!("{b:02x}")).collect();
        let json = serde_json::to_string(&value).unwrap();
        let json = json.trim_matches('"');
        let mid = value.len() / 2;
        let split = format!("{}\n{}", &value[..mid], &value[mid..]);
        let partial = &value[3..20];
        let forms = [
            ("hex", hex),
            ("json", json.to_string()),
            ("split", split),
            ("partial", partial.to_string()),
            ("raw", value.clone()),
        ];
        for (form, text) in forms {
            let mut response = EgressResponse {
                status_code: 200,
                headers: vec![("X-Echo".to_string(), format!("a {text} b"))]
                    .into_iter()
                    .collect(),
                body: [b"\xff\x00".as_slice(), text.as_bytes(), b"\xfe".as_slice()].concat(),
                error: None,
            };
            scrub_response(&mut response, std::slice::from_ref(&binding), &provider);
            let header = response.headers.get("X-Echo").unwrap();
            assert_eq!(header, &format!("a {placeholder} b"), "header, form {form}");
            let want = [
                b"\xff\x00".as_slice(),
                placeholder.as_bytes(),
                b"\xfe".as_slice(),
            ]
            .concat();
            assert_eq!(response.body, want, "body, form {form}");
        }
    }

    #[test]
    fn placeholder_over_plain_http_is_refused_before_injection() {
        let placeholder = generate_placeholder("sk_live_real_value", "urn:vam:run:test/api", "api");
        let binding = SecretBinding {
            binding_id: "api".to_string(),
            placeholder: placeholder.clone(),
            secret_ref: "prod/api".to_string(),
            allowed_destinations: vec!["api.example.com".to_string()],
            inject_at: Some(InjectionSite {
                r#type: vam_spec::v1::InjectionSiteType::InjectionSiteHeader as i32,
                key: "Authorization".to_string(),
            }),
        };
        let plan = SecretInjectionPlan::default();
        let request = |url: &str| EgressRequest {
            method: "GET".to_string(),
            url: url.to_string(),
            headers: vec![("authorization".to_string(), format!("Bearer {placeholder}"))]
                .into_iter()
                .collect(),
            body: Vec::new(),
        };

        let err = refuse_placeholders_over_plain_http(
            &request("http://api.example.com/v1/models"),
            std::slice::from_ref(&binding),
            &plan,
            "http",
        )
        .unwrap_err();
        assert!(err.to_string().contains("refused over plain HTTP"));

        refuse_placeholders_over_plain_http(
            &request("https://api.example.com/v1/models"),
            std::slice::from_ref(&binding),
            &plan,
            "https",
        )
        .unwrap();

        // A request that carries no placeholder is not a secret-bearing request.
        let mut plain = request("http://api.example.com/v1/models");
        plain
            .headers
            .insert("authorization".to_string(), "Bearer public".to_string());
        refuse_placeholders_over_plain_http(&plain, std::slice::from_ref(&binding), &plan, "http")
            .unwrap();

        // A URL-site substitution performed in phase 1 is also a secret on the wire.
        let url_plan = SecretInjectionPlan {
            url_substituted: vec![0],
        };
        let no_placeholder = EgressRequest {
            method: "GET".to_string(),
            url: "http://api.example.com/v1/models?key=sk_live_real_value".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        assert!(refuse_placeholders_over_plain_http(
            &no_placeholder,
            std::slice::from_ref(&binding),
            &url_plan,
            "http"
        )
        .is_err());
    }
}
