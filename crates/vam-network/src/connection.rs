//! The per-call egress envelope for connection bindings (CN-2.2, CN-2.3).
//!
//! The pure screening (the shapes, the call window, [`Envelope::screen`] and the path rules) lives in
//! `vam_spec::connection_envelope` so the `vam connector test` kit runs the gate's own rules offline
//! (CN-7.5); it is re-exported here, so the gate's call sites are unchanged. The token-source and
//! write-ticket traits, `set_bearer` and `scrub_secret` stay in this crate.

use async_trait::async_trait;
use std::collections::HashMap;
use vam_spec::v1::EgressResponse;

pub use vam_spec::connection_envelope::*;

/// Where a connection token is attached. Implemented by the orchestrator.
#[async_trait]
pub trait ConnectionTokenSource: Send + Sync {
    /// Pure screen, before DNS and before any secret is touched. `Ok(None)` means the request
    /// carries no connection placeholder and takes the static path.
    fn authorize(
        &self,
        request: &ConnectionRequest<'_>,
    ) -> Result<Option<ConnectionGrant>, ConnectionRefusal>;

    /// Called only after the destination was allowed. Re-checks the grant against the window,
    /// reads the token and puts it in the request headers.
    async fn attach(
        &self,
        grant: &ConnectionGrant,
        headers: &mut HashMap<String, String>,
    ) -> Result<(), ConnectionRefusal>;

    /// Removes the tokens this source attached from a response.
    fn scrub(&self, response: &mut EgressResponse);

    /// Called after `authorize` and before `attach`, for every admitted request. A source that
    /// journals writes returns a ticket for one (and refuses when the journal cannot be
    /// written: nothing is attached and nothing is sent); a read, or a source that journals
    /// nothing, returns `None`. The caller must call [`WriteTicket::commit_send`] immediately
    /// before the request leaves the host and send only if it returns `Ok`.
    async fn reserve_write(
        &self,
        _grant: &ConnectionGrant,
    ) -> Result<Option<Box<dyn WriteTicket>>, ConnectionRefusal> {
        Ok(None)
    }

    /// True when `host` is a destination this source can attach a token to, whether or not a
    /// given request carries a placeholder. The gate marks such a host secret-bound
    /// (`PreparedEgress::secret_bound`): a response from it that the proxy could not scrub is
    /// refused, never returned. Required, so an implementation cannot forget it.
    fn binds_host(&self, host: &str) -> bool;
}

/// A journaled write the token source reserved at the envelope (CN-4.7). The ordering the
/// gate keeps: the intent was written before the call was authorized; `commit_send` writes
/// `sent` immediately before the request leaves the host; `answered` records the far end's
/// answer. A ticket that is dropped after `commit_send` without an answer leaves the call
/// `unknown` (the request may have been received); dropped before it, nothing was sent and
/// the journal is unchanged.
pub trait WriteTicket: Send {
    /// Persists that the request is about to leave. An `Err` means it must not be sent.
    fn commit_send(&mut self) -> Result<(), ConnectionRefusal>;

    /// The far end answered with this HTTP status.
    fn answered(&mut self, status: u16);
}

/// Sets the one `Authorization` header to `Bearer <token>`, dropping every other spelling.
pub fn set_bearer(headers: &mut HashMap<String, String>, token: &str) {
    headers.retain(|k, _| !k.eq_ignore_ascii_case("authorization"));
    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
}

/// Sets the one header an `inject: header` binding names to `<prefix><value>`, dropping every other
/// letter case of that name first, so the key is in exactly one header and the guest's own spelling
/// of it is gone. `name` and `prefix` come from the connector file's binding, never from a request.
/// A name that is not a plain header name, or is one a key must never be put in, is refused here as
/// well as at validation: nothing is set then. The rule itself is `vam_spec::connection_envelope::
/// inject_header`, which `vam connector test` runs too (it cannot depend on this crate).
pub fn set_header(
    headers: &mut HashMap<String, String>,
    name: &str,
    prefix: &str,
    value: &str,
) -> Result<(), ConnectionRefusal> {
    inject_header(headers, name, prefix, value)
}

/// Replaces `secret` in a response with `placeholder`, in headers and body (every encoded form
/// the static scrub covers).
pub fn scrub_secret(response: &mut EgressResponse, secret: &str, placeholder: &str) {
    if secret.is_empty() {
        return;
    }
    let redactor = vam_security::secret_forms::SecretRedactor::with_replacement(
        &[secret.as_bytes()],
        placeholder,
    );
    for val in response.headers.values_mut() {
        if let std::borrow::Cow::Owned(scrubbed) = redactor.redact_str(val) {
            *val = scrubbed;
        }
    }
    if let std::borrow::Cow::Owned(scrubbed) = redactor.redact_bytes(&response.body) {
        response.body = scrubbed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PH: &str = "vamsec_m365_graph";

    #[test]
    fn set_bearer_leaves_one_authorization_header() {
        let mut h = HashMap::from([
            ("authorization".to_string(), PH.to_string()),
            ("AUTHORIZATION".to_string(), PH.to_string()),
            ("X-Keep".to_string(), "1".to_string()),
        ]);
        set_bearer(&mut h, "tok");
        assert_eq!(h.len(), 2);
        assert_eq!(
            h.get("Authorization").map(String::as_str),
            Some("Bearer tok")
        );
    }

    fn map(h: &[(&str, &str)]) -> HashMap<String, String> {
        h.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn set_header_leaves_one_header_of_that_name_and_touches_no_other() {
        let mut h = map(&[
            ("x-api-key", "vamsec_a_b"),
            ("X-API-KEY", "vamsec_a_b"),
            ("X-Api-Key", "guest"),
            ("Authorization", "Bearer guest-own"),
            ("X-Keep", "1"),
        ]);
        set_header(&mut h, "X-Api-Key", "", "KEY-VALUE").unwrap();
        assert_eq!(h.len(), 3, "{h:?}");
        assert_eq!(h["X-Api-Key"], "KEY-VALUE");
        // Every other header is as the guest sent it, and none holds the key.
        assert_eq!(h["Authorization"], "Bearer guest-own");
        assert_eq!(h["X-Keep"], "1");
        assert_eq!(h.values().filter(|v| v.contains("KEY-VALUE")).count(), 1);
    }

    #[test]
    fn set_header_puts_the_prefix_before_the_key() {
        let mut h = map(&[("x-token", "vamsec_a_b")]);
        set_header(&mut h, "X-Token", "Token ", "KEY").unwrap();
        assert_eq!(h, map(&[("X-Token", "Token KEY")]));
        // The name is the file's spelling; a header added after is a different header.
        let mut h = HashMap::new();
        set_header(&mut h, "ocp-apim-subscription-key", "", "KEY").unwrap();
        assert_eq!(h["ocp-apim-subscription-key"], "KEY");
    }

    #[test]
    fn set_header_refuses_a_name_a_key_must_never_be_put_in_and_changes_nothing() {
        for name in [
            "Host",
            "authorization",
            "Cookie",
            "Proxy-Authorization",
            "proxy-x",
            "Connection",
            "Content-Length",
            "Transfer-Encoding",
            "Keep-Alive",
            "TE",
            "Trailer",
            "Upgrade",
            "",
            "X Y",
            "X:Y",
            "X\r\nHost",
            "X-Ключ",
        ] {
            let mut h = map(&[("Authorization", "Bearer guest"), ("Host", "h")]);
            let before = h.clone();
            let e = set_header(&mut h, name, "", "KEY").unwrap_err();
            assert!(
                e.text.starts_with("Connection credential refused"),
                "{name}"
            );
            assert!(!e.text.contains("KEY"));
            assert_eq!(h, before, "{name}: headers changed");
        }
        // A prefix or a value with a control character (header injection) is refused too.
        for (p, v) in [("a\r\nX-Evil: 1", "KEY"), ("", "KE\nY"), ("", "KE\r\nY")] {
            let mut h = map(&[("X-Keep", "1")]);
            assert!(set_header(&mut h, "X-Api-Key", p, v).is_err());
            assert_eq!(h, map(&[("X-Keep", "1")]));
        }
    }

    #[test]
    fn scrub_covers_the_key_and_the_prefixed_header_value() {
        let key = "ndk_live_0123456789abcdef";
        let injected = format!("Token {key}");
        let mut resp = EgressResponse {
            status_code: 200,
            headers: map(&[("X-Echo", &format!("seen {injected}")), ("X-Key-Only", key)])
                .into_iter()
                .collect(),
            body: format!("{injected} and {key}").into_bytes(),
            error: None,
        };
        // Longest first, as the token source does: the whole header value, then the key alone.
        scrub_secret(&mut resp, &injected, "vamsec_n_api");
        scrub_secret(&mut resp, key, "vamsec_n_api");
        let body = String::from_utf8(resp.body.clone()).unwrap();
        assert!(!body.contains(key) && !body.contains("Token "), "{body}");
        for v in resp.headers.values() {
            assert!(!v.contains(key), "{v}");
        }
        assert!(body.contains("vamsec_n_api"));
    }
}
