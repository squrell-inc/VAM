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
}
