//! Egress policy gate for the TLS-terminating proxy (E8-P1).
//!
//! [`crate::broker::BrokerGate`] is the shared implementation used by the
//! legacy port-54 broker and the port-55 HTTP proxy.

use async_trait::async_trait;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use thiserror::Error;
use vam_spec::v1::{EgressRequest, EgressResponse};

/// How the proxy handles traffic to an allowed destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    /// Terminate TLS from the guest and re-encrypt to the upstream origin.
    Terminate,
    /// Forward a `CONNECT` tunnel without MITM.
    Passthrough,
}

/// Result of a destination check: where to dial and how to handle the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateDecision {
    pub addr: SocketAddr,
    pub mode: GateMode,
}

#[derive(Debug, Error)]
pub enum GateError {
    #[error("denied: {0}")]
    Denied(String),
    #[error("quota exceeded")]
    QuotaExceeded,
    #[error("name resolution failed: {0}")]
    Resolution(String),
    #[error("internal error: {0}")]
    Internal(String),
}

/// A request after the gate has run: the request to send, whether the destination holds a
/// secret for this session, and the journal ticket when the request is a journaled write.
pub struct PreparedEgress {
    pub request: EgressRequest,
    /// True when the destination host has a secret binding in this session, or a
    /// secret was substituted into this request. The proxy must not return any
    /// response from such a host that it could not scrub (see `proxy::mitm`), and
    /// it does not need a placeholder in the request to know that: an upstream can
    /// hand back a secret it was sent earlier.
    pub secret_bound: bool,
    /// A journaled write (CN-4.7) the caller must commit immediately before sending and answer
    /// with the response status; `None` for a read, and for a gate that journals nothing. A
    /// caller that sends nothing itself uses [`EgressGate::prepare`], which refuses a request
    /// that comes with a ticket.
    pub ticket: Option<Box<dyn crate::connection::WriteTicket>>,
}

impl std::fmt::Debug for PreparedEgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedEgress")
            .field("request", &self.request)
            .field("secret_bound", &self.secret_bound)
            .field("ticket", &self.ticket.is_some())
            .finish()
    }
}

/// Why the terminating proxy refused to return an upstream response.
pub const REFUSED_OVERSIZE_WITH_SECRET: &str = "oversize_with_secret";

/// Session-scoped egress policy: allowlists, quotas, and secret injection hooks.
#[async_trait]
pub trait EgressGate: Send + Sync {
    /// DNS, IP deny set, host and port allowlist. Returns the pinned address to dial.
    async fn check_destination(
        &self,
        host: &str,
        port: u16,
        scheme: &str,
    ) -> Result<GateDecision, GateError>;

    /// Quota, secret placeholder substitution and injection for one HTTP request,
    /// and whether the destination holds a session secret. Required, so a gate
    /// cannot leave the answer out and be treated as having no secret.
    async fn prepare_request(&self, req: EgressRequest) -> Result<PreparedEgress, GateError>;

    /// [`Self::prepare_request`] without the secret flag, for callers that buffer
    /// the whole response (plain HTTP and the port-54 broker) and cannot commit a journal
    /// ticket: a journaled write is refused here (nothing was sent, the ticket is dropped
    /// uncommitted), never handed to a caller that would send it unjournaled.
    async fn prepare(&self, req: EgressRequest) -> Result<EgressRequest, GateError> {
        let prepared = self.prepare_request(req).await?;
        if prepared.ticket.is_some() {
            return Err(GateError::Internal(
                "a journaled write needs a caller that commits it before sending".to_string(),
            ));
        }
        Ok(prepared.request)
    }

    /// Response scrubbing and byte accounting.
    async fn finish(&self, resp: EgressResponse) -> Result<EgressResponse, GateError>;

    /// Count tunnel bytes against the session quota (`CONNECT` passthrough).
    fn record_bytes(&self, n: u64) -> Result<(), GateError>;

    /// The proxy refused to return a response from `host` (`reason` is one of the
    /// `REFUSED_*` constants). Observability only: the refusal has already happened.
    fn response_refused(&self, _host: &str, _port: u16, _reason: &str) {}
}

/// Outcome of the per-host egress authorization (E1 AX-5.3d).
///
/// `rule` names what decided: a host rule (`agent rule *.pastebin.com`), the
/// blanket `internet:*`, or a fail-closed reason. It is recorded on the
/// `egress.decided` event and the OCSF block event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressAuthorization {
    Allow { rule: String },
    Deny { rule: String },
}

/// Decides whether the session may reach a host name.
///
/// [`crate::broker::BrokerGate`] calls this after DNS and the platform IP floor
/// ([`crate::policy::is_ip_denied`]), so an authorizer can never widen access
/// to a protected address. Implementations must fail closed: if the policy
/// cannot be evaluated they return [`EgressAuthorization::Deny`]. `vam-network`
/// stays free of any policy engine; `vamd` supplies a Cedar implementation.
pub trait EgressAuthorizer: Send + Sync {
    /// `host` is lower-cased with trailing dots removed.
    fn authorize(&self, host: &str) -> EgressAuthorization;
}

/// Turns a host name into socket addresses.
///
/// Production uses [`SystemResolver`]. Tests inject a fixed map so policy
/// checks never depend on live DNS. Same shape as
/// `vam_security::ssrf_guard::HostResolver`.
pub trait HostResolver: Send + Sync {
    fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

/// Resolves through the operating system (`getaddrinfo`).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemResolver;

impl HostResolver for SystemResolver {
    fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        (host, port).to_socket_addrs().map(Iterator::collect)
    }
}
