use async_trait::async_trait;
use bytes::Bytes;
use prost::Message;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use vam_spec::v1::{
    EgressRequest, EgressResponse, ErrorStatus, SecretBinding, VamErrorCode, VsockEnvelope,
};
use vam_vsock::frame::{Frame, FrameType, MAX_PAYLOAD_SIZE};

use crate::connection::{ConnectionGrant, ConnectionRequest, ConnectionTokenSource};
use crate::error::VamNetworkError;
use crate::gate::{
    EgressAuthorization, EgressAuthorizer, EgressGate, GateDecision, GateError, GateMode,
    HostResolver, PreparedEgress, SystemResolver,
};
use crate::policy::{
    allow_destination, allow_port, parse_ip_literal, ActivePolicyContainer, Decision,
};
use crate::secrets::{
    destination_holds_secret, phase1_substitute_url_secrets, phase2_inject_secrets,
    refuse_placeholders_over_plain_http, scrub_response, SecretInjectionPlan, SecretProvider,
};

const LEGACY_CHANNEL_OVERSIZE: &str =
    "response too large for the legacy egress channel; use HTTPS_PROXY";

pub fn make_envelope<T: Message>(method: &str, correlation_id: u32, msg: &T) -> VsockEnvelope {
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    VsockEnvelope {
        method: method.to_string(),
        correlation_id,
        payload,
        error: None,
        trace_context: None,
    }
}

pub fn make_error_envelope(
    method: &str,
    correlation_id: u32,
    code: VamErrorCode,
    message: &str,
) -> VsockEnvelope {
    VsockEnvelope {
        method: method.to_string(),
        correlation_id,
        payload: Vec::new(),
        error: Some(ErrorStatus {
            code: code as i32,
            message: message.to_string(),
            retryable: false,
            details: Vec::new(),
        }),
        trace_context: None,
    }
}

pub struct EgressBrokerClient {
    request_tx: mpsc::Sender<Frame>,
    pending_requests: Arc<Mutex<HashMap<u32, oneshot::Sender<VsockEnvelope>>>>,
    next_correlation_id: AtomicU64,
}

impl EgressBrokerClient {
    pub fn new<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (request_tx, mut request_rx) = mpsc::channel::<Frame>(128);
        let pending_requests = Arc::new(Mutex::new(
            HashMap::<u32, oneshot::Sender<VsockEnvelope>>::new(),
        ));
        let pending_requests_clone = pending_requests.clone();

        // Spawn writer task
        tokio::spawn(async move {
            while let Some(frame) = request_rx.recv().await {
                if let Err(e) = frame.write_to(&mut writer).await {
                    eprintln!("EgressBrokerClient writer error: {}", e);
                    break;
                }
            }
        });

        // Spawn reader task
        tokio::spawn(async move {
            loop {
                match Frame::read_from(&mut reader).await {
                    Ok(frame) => {
                        if frame.frame_type != FrameType::StreamEgress {
                            continue;
                        }
                        if let Ok(envelope) = VsockEnvelope::decode(frame.payload.as_ref()) {
                            let mut map = pending_requests_clone.lock().unwrap();
                            if let Some(tx) = map.remove(&frame.correlation_id) {
                                let _ = tx.send(envelope);
                            }
                        }
                    }
                    Err(e) => {
                        if e.kind() != io::ErrorKind::UnexpectedEof {
                            eprintln!("EgressBrokerClient reader error: {}", e);
                        }
                        break;
                    }
                }
            }
        });

        Self {
            request_tx,
            pending_requests,
            next_correlation_id: AtomicU64::new(1),
        }
    }

    pub async fn dispatch(&self, req: &EgressRequest) -> Result<EgressResponse, VamNetworkError> {
        let correlation_id = self.next_correlation_id.fetch_add(1, Ordering::SeqCst) as u32;
        let mut req_bytes = Vec::new();
        req.encode(&mut req_bytes)
            .map_err(|e| VamNetworkError::Serialization(e.to_string()))?;

        let envelope = VsockEnvelope {
            method: "Egress".to_string(),
            correlation_id,
            payload: req_bytes,
            error: None,
            trace_context: None,
        };

        let mut env_bytes = Vec::new();
        envelope
            .encode(&mut env_bytes)
            .map_err(|e| VamNetworkError::Serialization(e.to_string()))?;

        let frame = Frame {
            frame_type: FrameType::StreamEgress,
            correlation_id,
            payload: Bytes::from(env_bytes),
        };

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending_requests.lock().unwrap();
            map.insert(correlation_id, tx);
        }

        self.request_tx
            .send(frame)
            .await
            .map_err(|_| VamNetworkError::ConnectionClosed)?;

        let response_env = rx.await.map_err(|_| VamNetworkError::ConnectionClosed)?;

        if let Some(err) = response_env.error {
            return Ok(EgressResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Vec::new(),
                error: Some(err),
            });
        }

        let resp = EgressResponse::decode(response_env.payload.as_slice())
            .map_err(|e| VamNetworkError::Serialization(e.to_string()))?;

        Ok(resp)
    }
}

pub type EgressDecisionCallback = Arc<dyn Fn(&str, u32, &str, &str) + Send + Sync>;

/// Session egress policy: DNS pinning, deny set, allowlist, quota, and secrets.
pub struct BrokerGate {
    policy_container: Arc<ActivePolicyContainer>,
    secret_provider: Arc<dyn SecretProvider>,
    bindings: Arc<Mutex<Vec<SecretBinding>>>,
    total_egress_bytes: Arc<AtomicU64>,
    max_egress_bytes: Arc<AtomicU64>,
    decision_callback: Option<EgressDecisionCallback>,
    resolver: Arc<dyn HostResolver>,
    /// CONNECT targets that stay passthrough even on port 443 (`egress.passthroughHosts`).
    passthrough_hosts: Vec<String>,
    /// Hermetic tests set this so an injected resolver can pin a name at a
    /// local server. `vamd` never sets it.
    exempt_ip_deny: bool,
    /// Host decision after the IP floor. `None` keeps the snapshot's
    /// `allowed_domains` allowlist (hermetic network tests).
    authorizer: Option<Arc<dyn EgressAuthorizer>>,
    /// TEST NODES ONLY (`process-guest`): the one host name that dials the E2E
    /// fixture origin instead of resolving. It still passes the host authorizer
    /// and the port check; only DNS and the IP floor are skipped for it.
    #[cfg(feature = "process-guest")]
    fixture_origin: Option<(String, SocketAddr)>,
    /// Where connection placeholders are swapped for the member's token, inside the calling
    /// tool's declared shape. `None` leaves every placeholder to the static bindings.
    connection_tokens: Option<Arc<dyn ConnectionTokenSource>>,
    /// TEST ONLY (process-guest builds): one host answered by a loopback http server.
    #[cfg(feature = "process-guest")]
    upstream_redirect: Option<UpstreamRedirect>,
}

/// TEST ONLY: `host` is served by `base` (plain http on a loopback IP). It stands in for the
/// provider host in a process-guest node; the authorizer and port check still run for `host`.
#[cfg(feature = "process-guest")]
#[derive(Debug, Clone)]
pub struct UpstreamRedirect {
    host: String,
    base: reqwest::Url,
    addr: SocketAddr,
}

/// A port-54 request that failed, with the rule when the destination check
/// refused it. The rule rides to the `egress.decided` event.
struct EgressFailure {
    error: VamNetworkError,
    rule: Option<String>,
    /// A connection-envelope refusal. Port 55 reports these itself (`prepare`).
    envelope: bool,
}

impl From<VamNetworkError> for EgressFailure {
    fn from(error: VamNetworkError) -> Self {
        Self {
            error,
            rule: None,
            envelope: false,
        }
    }
}

impl From<crate::connection::ConnectionRefusal> for EgressFailure {
    fn from(refusal: crate::connection::ConnectionRefusal) -> Self {
        Self {
            error: VamNetworkError::EgressForbidden {
                reason: refusal.text,
            },
            rule: Some(refusal.rule),
            envelope: true,
        }
    }
}

/// What `prepare_with_decision` hands the two senders (port 54's broker and the gate's
/// `prepare_request`).
struct PreparedCall {
    req: EgressRequest,
    decision: GateDecision,
    rule: String,
    /// The destination holds a session secret (`gate::PreparedEgress::secret_bound`).
    secret_bound: bool,
    /// A journaled write the caller must commit before sending (CN-4.7).
    ticket: Option<Box<dyn crate::connection::WriteTicket>>,
}

/// A refused destination and the rule that refused it.
struct Denial {
    error: GateError,
    rule: String,
}

impl BrokerGate {
    pub fn new(
        policy_container: Arc<ActivePolicyContainer>,
        secret_provider: Arc<dyn SecretProvider>,
        bindings: Arc<Mutex<Vec<SecretBinding>>>,
    ) -> Self {
        Self {
            policy_container,
            secret_provider,
            bindings,
            total_egress_bytes: Arc::new(AtomicU64::new(0)),
            max_egress_bytes: Arc::new(AtomicU64::new(0)),
            decision_callback: None,
            resolver: Arc::new(SystemResolver),
            passthrough_hosts: Vec::new(),
            exempt_ip_deny: false,
            authorizer: None,
            #[cfg(feature = "process-guest")]
            fixture_origin: None,
            connection_tokens: None,
            #[cfg(feature = "process-guest")]
            upstream_redirect: None,
        }
    }

    /// Dial `addr` for `host` (the E2E fixture origin). Test nodes only.
    #[cfg(feature = "process-guest")]
    pub fn with_fixture_origin(mut self, host: &str, addr: SocketAddr) -> Self {
        self.fixture_origin = Some((normalize_host(host), addr));
        self
    }

    /// Swap connection placeholders for tokens through `source`, inside a tool's declared shape.
    pub fn with_connection_tokens(mut self, source: Arc<dyn ConnectionTokenSource>) -> Self {
        self.connection_tokens = Some(source);
        self
    }

    /// TEST ONLY: serve `host` from `base`, a plain-http loopback URL (`http://127.0.0.1:P`).
    /// Anything else is ignored. The host still goes through the authorizer and port check.
    #[cfg(feature = "process-guest")]
    pub fn with_upstream_redirect(mut self, host: &str, base: &str) -> Self {
        let Ok(base) = reqwest::Url::parse(base) else {
            return self;
        };
        let Some(port) = base.port() else {
            return self;
        };
        let ip = base
            .host_str()
            .and_then(|h| h.trim_matches(['[', ']']).parse::<IpAddr>().ok());
        let Some(ip) = ip.filter(IpAddr::is_loopback) else {
            return self;
        };
        if base.scheme() != "http" || !base.username().is_empty() {
            return self;
        }
        self.upstream_redirect = Some(UpstreamRedirect {
            host: normalize_host(host),
            base,
            addr: SocketAddr::new(ip, port),
        });
        self
    }

    #[cfg(feature = "process-guest")]
    fn fixture_addr_for(&self, host: &str) -> Option<SocketAddr> {
        let (name, addr) = self.fixture_origin.as_ref()?;
        (normalize_host(host) == *name).then_some(*addr)
    }

    #[cfg(not(feature = "process-guest"))]
    fn fixture_addr_for(&self, _host: &str) -> Option<SocketAddr> {
        None
    }

    #[cfg(feature = "process-guest")]
    fn redirect_for(&self, host: &str) -> Option<&UpstreamRedirect> {
        self.upstream_redirect
            .as_ref()
            .filter(|r| r.host == normalize_host(host))
    }

    /// Route every host decision, on port 54 and port 55 alike, through
    /// `authorizer`. It runs after DNS and the platform IP floor, which stay
    /// in Rust, so it can only narrow what the floor already allows.
    pub fn with_authorizer(mut self, authorizer: Arc<dyn EgressAuthorizer>) -> Self {
        self.authorizer = Some(authorizer);
        self
    }

    pub fn with_resolver(mut self, resolver: Arc<dyn HostResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    pub fn with_decision_callback(mut self, callback: EgressDecisionCallback) -> Self {
        self.decision_callback = Some(callback);
        self
    }

    /// Hosts that force `CONNECT` passthrough, including on port 443.
    pub fn with_passthrough_hosts(mut self, hosts: Vec<String>) -> Self {
        self.passthrough_hosts = hosts;
        self
    }

    /// Skip the protected-destination deny set. Hermetic tests use this with
    /// [`Self::with_resolver`] so a local server can stand in for an allowed host.
    pub fn with_ip_deny_exemption(mut self) -> Self {
        self.exempt_ip_deny = true;
        self
    }

    /// Per-session cap on bytes moved through this gate, across CONNECT
    /// tunnels, MITM bodies and plain HTTP. `0` means unlimited.
    pub fn with_max_egress_bytes(self, max_bytes: u64) -> Self {
        self.max_egress_bytes.store(max_bytes, Ordering::SeqCst);
        self
    }

    fn bindings(&self) -> std::sync::MutexGuard<'_, Vec<SecretBinding>> {
        self.bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn enforce_quota(&self, req_len: u64) -> Result<(), VamNetworkError> {
        let cap = self.max_egress_bytes.load(Ordering::SeqCst);
        let current = self.total_egress_bytes.load(Ordering::SeqCst);
        if cap > 0 && (current + req_len) > cap {
            let err = format!(
                "Egress byte quota exceeded: limit={}, attempted={}",
                cap,
                current + req_len
            );
            eprintln!("[egress:trace] Quota Error: {}", err);
            return Err(VamNetworkError::EgressForbidden { reason: err });
        }
        Ok(())
    }

    fn phase1(&self, req: &mut EgressRequest) -> Result<SecretInjectionPlan, VamNetworkError> {
        let bindings = self.bindings();
        match phase1_substitute_url_secrets(req, &bindings, &*self.secret_provider) {
            Ok(plan) => {
                eprintln!("[egress:trace] Secret Injection Phase 1 URL: {}", req.url);
                Ok(plan)
            }
            Err(err) => {
                eprintln!("[egress:trace] Secret Injection Phase 1 Error: {}", err);
                Err(err)
            }
        }
    }

    fn phase2(
        &self,
        req: &mut EgressRequest,
        host: &str,
        plan: &SecretInjectionPlan,
    ) -> Result<(), VamNetworkError> {
        let bindings = self.bindings();
        if let Err(err) = phase2_inject_secrets(req, &bindings, &*self.secret_provider, host, plan)
        {
            eprintln!("[egress:trace] Secret Injection Phase 2 Error: {}", err);
            return Err(err);
        }
        Ok(())
    }

    fn scrub(&self, resp: &mut EgressResponse) {
        {
            let bindings = self.bindings();
            scrub_response(resp, &bindings, &*self.secret_provider);
        }
        if let Some(source) = &self.connection_tokens {
            source.scrub(resp);
        }
    }

    fn destination_ip_denied(&self, ip: &IpAddr) -> bool {
        if self.exempt_ip_deny {
            return false;
        }
        crate::policy::is_ip_denied(ip)
    }

    async fn lookup_host(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, GateError> {
        if let Some(ip) = parse_ip_literal(host) {
            return Ok(vec![SocketAddr::new(ip, port)]);
        }
        let resolver = Arc::clone(&self.resolver);
        let host_owned = host.to_string();
        let addrs = tokio::task::spawn_blocking(move || resolver.lookup(&host_owned, port))
            .await
            .map_err(|err| GateError::Internal(format!("resolver task failed: {err}")))?
            .map_err(|err| {
                let message = format!("Failed to resolve host '{}': {}", host, err);
                eprintln!("[egress:trace] DNS Resolution Error: {}", message);
                GateError::Resolution(message)
            })?;
        if addrs.is_empty() {
            let message = format!("No IP addresses found for host '{}'", host);
            eprintln!("[egress:trace] DNS No IP Found: {}", message);
            return Err(GateError::Resolution(message));
        }
        Ok(addrs)
    }

    /// DNS, platform IP floor, host decision, port check. Returns the pinned
    /// address and the rule that allowed it, or the refusal and its rule.
    async fn check_destination_inner(
        &self,
        host: &str,
        port: u16,
        scheme: &str,
    ) -> Result<(GateDecision, String), Denial> {
        eprintln!("[egress:trace] Resolving host {}:{}...", host, port);
        let fixture = self.fixture_addr_for(host);
        // TEST ONLY: the one redirected host is pinned to its loopback server. The authorizer
        // and the port check below still decide; only DNS and the IP floor are skipped.
        #[cfg(feature = "process-guest")]
        let redirected = self.redirect_for(host).map(|r| r.addr);
        #[cfg(not(feature = "process-guest"))]
        let redirected: Option<SocketAddr> = None;
        let pinned_override = fixture.or(redirected);
        let lookup = match pinned_override {
            Some(addr) => Ok(vec![addr]),
            None => self.lookup_host(host, port).await,
        };
        let addrs = match lookup {
            Ok(addrs) => addrs,
            Err(error) => {
                // No address means no IP floor to apply. If the session's rules
                // refuse this host anyway, report that rule rather than the DNS
                // failure, so the event names what the policy decided.
                if let Some(EgressAuthorization::Deny { rule }) = self.authorize_host(host) {
                    return Err(Denial {
                        error: GateError::Denied(denied_by_rule_message(host, &rule)),
                        rule,
                    });
                }
                return Err(Denial {
                    error,
                    rule: "name resolution".to_string(),
                });
            }
        };

        let mut pinned_ip = None;
        for addr in &addrs {
            let ip = addr.ip();
            if pinned_override.is_none() && self.destination_ip_denied(&ip) {
                let err = format!(
                    "Egress target IP '{}' is in the Protected Destination Deny Set",
                    ip
                );
                eprintln!("[egress:trace] SSRF Protection Blocked: {}", err);
                return Err(Denial {
                    error: GateError::Denied(err),
                    rule: PLATFORM_IP_FLOOR_RULE.to_string(),
                });
            }
            if pinned_ip.is_none() {
                pinned_ip = Some(ip);
            }
        }
        let Some(pinned_ip) = pinned_ip else {
            let message = format!("No IP addresses found for host '{}'", host);
            eprintln!("[egress:trace] DNS No IP Found: {}", message);
            return Err(Denial {
                error: GateError::Resolution(message),
                rule: "name resolution".to_string(),
            });
        };
        eprintln!("[egress:trace] Resolved IP {} -> {}", host, pinned_ip);

        let snapshot = self.policy_container.load();
        let rule = match &self.authorizer {
            Some(authorizer) => match authorizer.authorize(&normalize_host(host)) {
                EgressAuthorization::Allow { rule } => {
                    if let Decision::Deny(reason) = allow_port(&snapshot, host, port) {
                        eprintln!("[egress:trace] Port Check Failed: {}", reason);
                        return Err(Denial {
                            error: GateError::Denied(reason),
                            rule: PORT_ALLOWLIST_RULE.to_string(),
                        });
                    }
                    rule
                }
                EgressAuthorization::Deny { rule } => {
                    let reason = denied_by_rule_message(host, &rule);
                    eprintln!("[egress:trace] Policy Check Failed: {}", reason);
                    return Err(Denial {
                        error: GateError::Denied(reason),
                        rule,
                    });
                }
            },
            None => {
                if let Decision::Deny(reason) = allow_destination(&snapshot, host, port) {
                    eprintln!("[egress:trace] Policy Check Failed: {}", reason);
                    return Err(Denial {
                        error: GateError::Denied(reason),
                        rule: String::new(),
                    });
                }
                String::new()
            }
        };

        let mode = if scheme.eq_ignore_ascii_case("connect") {
            connect_gate_mode(host, port, &self.passthrough_hosts)
        } else {
            GateMode::Terminate
        };
        let addr = match pinned_override {
            Some(addr) => addr,
            None => SocketAddr::new(pinned_ip, port),
        };
        Ok((GateDecision { addr, mode }, rule))
    }

    /// The authorizer's answer for `host`, or `None` without one.
    fn authorize_host(&self, host: &str) -> Option<EgressAuthorization> {
        self.authorizer
            .as_ref()
            .map(|authorizer| authorizer.authorize(&normalize_host(host)))
    }

    fn notify_decision(
        &self,
        host: &str,
        port: u16,
        disposition: &str,
        mode: GateMode,
        rule: &str,
    ) {
        let Some(callback) = &self.decision_callback else {
            return;
        };
        let mode_label = match mode {
            GateMode::Terminate => "terminate",
            GateMode::Passthrough => "passthrough",
        };
        let policy_id = if disposition.eq_ignore_ascii_case("DENY") {
            format!("active-policy:{mode_label}")
        } else {
            let generation = self.policy_container.load().policy_generation;
            format!("gen-{generation}:{mode_label}")
        };
        callback(
            host,
            u32::from(port),
            disposition,
            &decision_label(&policy_id, rule),
        );
    }

    /// Quota, phase 1, destination check, then phase 2.
    ///
    /// Phase 2 runs only after the destination is allowed, so a policy or
    /// deny-set rejection does not inject header or body secrets.
    async fn prepare_with_decision(
        &self,
        mut req: EgressRequest,
    ) -> Result<PreparedCall, EgressFailure> {
        let req_len = (req.url.len() + req.body.len()) as u64;
        self.enforce_quota(req_len)?;

        eprintln!(
            "[egress:trace] Incoming Guest Egress {} {}",
            req.method, req.url
        );

        let plan = self.phase1(&mut req)?;
        let url = reqwest::Url::parse(&req.url).map_err(|err| {
            let message = format!(
                "Invalid effective URL after secret injection '{}': {}",
                req.url, err
            );
            eprintln!("[egress:trace] Effective URL Parse Error: {}", message);
            VamNetworkError::EgressForbidden { reason: message }
        })?;
        let host = url
            .host_str()
            .ok_or_else(|| {
                eprintln!(
                    "[egress:trace] Effective URL must specify a host: {}",
                    req.url
                );
                VamNetworkError::EgressForbidden {
                    reason: "Effective URL must specify a host".to_string(),
                }
            })?
            .to_string();
        let port = url.port_or_known_default().unwrap_or(443);
        let scheme = url.scheme().to_string();
        {
            let bindings = self.bindings();
            refuse_placeholders_over_plain_http(&req, &bindings, &plan, &scheme)?;
        }

        // Connection placeholders: the envelope decides before DNS and before any token is
        // read. A request that carries one is either admitted for the active tool call or
        // refused here; it never reaches the static path or the upstream with a placeholder.
        let grant: Option<ConnectionGrant> = match &self.connection_tokens {
            Some(source) => source.authorize(&ConnectionRequest {
                method: &req.method,
                url: &req.url,
                headers: &req.headers,
                body: &req.body,
            })?,
            None => None,
        };

        let (decision, rule) = match self.check_destination_inner(&host, port, &scheme).await {
            Ok(allowed) => allowed,
            Err(denial) => {
                return Err(EgressFailure {
                    error: gate_err_to_net(denial.error),
                    rule: Some(denial.rule).filter(|rule| !rule.is_empty()),
                    envelope: false,
                });
            }
        };
        // A write is journaled before the token is read: a journal that cannot be written
        // refuses here, with nothing attached. The caller commits the ticket right before the
        // request leaves the host.
        let mut ticket = None;
        if let (Some(grant), Some(source)) = (&grant, &self.connection_tokens) {
            ticket = source.reserve_write(grant).await?;
            source.attach(grant, &mut req.headers).await?;
        }
        self.phase2(&mut req, &host, &plan)?;
        // Bound by destination, not by this request: see `PreparedEgress::secret_bound`.
        // A connection token counts the same way: this request carries one (`grant`), or the
        // destination is one a connection binding is pinned to (an upstream can echo a token it
        // was sent earlier, and `ConnectionTokenSource::scrub` cannot reach what the proxy
        // would not buffer).
        let connection_bound = grant.is_some()
            || self
                .connection_tokens
                .as_ref()
                .is_some_and(|source| source.binds_host(&host));
        let secret_bound = !plan.url_substituted.is_empty()
            || destination_holds_secret(&self.bindings(), &host)
            || connection_bound;
        Ok(PreparedCall {
            req,
            decision,
            rule,
            secret_bound,
            ticket,
        })
    }
}

#[async_trait]
impl EgressGate for BrokerGate {
    async fn check_destination(
        &self,
        host: &str,
        port: u16,
        scheme: &str,
    ) -> Result<GateDecision, GateError> {
        let fallback_mode = if scheme.eq_ignore_ascii_case("connect") {
            connect_gate_mode(host, port, &self.passthrough_hosts)
        } else {
            GateMode::Terminate
        };
        match self.check_destination_inner(host, port, scheme).await {
            Ok((decision, rule)) => {
                self.notify_decision(host, port, "ALLOW", decision.mode, &rule);
                Ok(decision)
            }
            Err(denial) => {
                self.notify_decision(host, port, "DENY", fallback_mode, &denial.rule);
                Err(denial.error)
            }
        }
    }

    async fn prepare_request(&self, req: EgressRequest) -> Result<PreparedEgress, GateError> {
        let target = reqwest::Url::parse(&req.url).ok().and_then(|u| {
            u.host_str()
                .map(|h| (h.to_string(), u.port_or_known_default().unwrap_or(443)))
        });
        match self.prepare_with_decision(req).await {
            Ok(prepared) => Ok(PreparedEgress {
                request: prepared.req,
                secret_bound: prepared.secret_bound,
                ticket: prepared.ticket,
            }),
            Err(failure) => {
                // Port 54 reports its refusals itself; the MITM path only gets the error.
                if failure.envelope {
                    if let (Some((host, port)), Some(rule)) = (target, failure.rule.as_deref()) {
                        self.notify_decision(&host, port, "DENY", GateMode::Terminate, rule);
                    }
                }
                Err(net_err_to_gate(failure.error))
            }
        }
    }

    fn response_refused(&self, host: &str, port: u16, reason: &str) {
        // A second `egress.decided` for the connection: DENY, with the reason as the rule.
        self.notify_decision(host, port, "DENY", GateMode::Terminate, reason);
    }

    async fn finish(&self, mut resp: EgressResponse) -> Result<EgressResponse, GateError> {
        let accounted = resp.body.len() as u64;
        self.scrub(&mut resp);
        EgressGate::record_bytes(self, accounted)?;
        Ok(resp)
    }

    fn record_bytes(&self, n: u64) -> Result<(), GateError> {
        // Check and add in one CAS loop so concurrent tunnels cannot overshoot
        // the cap. A refused chunk is not counted.
        let cap = self.max_egress_bytes.load(Ordering::SeqCst);
        let mut current = self.total_egress_bytes.load(Ordering::SeqCst);
        loop {
            let next = current.saturating_add(n);
            if cap > 0 && next > cap {
                eprintln!("[egress:trace] Quota Error: limit={cap}, attempted={next}");
                return Err(GateError::QuotaExceeded);
            }
            match self.total_egress_bytes.compare_exchange_weak(
                current,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }
}

pub struct EgressBrokerServer {
    gate: Arc<BrokerGate>,
}

impl EgressBrokerServer {
    pub fn new(
        policy_container: Arc<ActivePolicyContainer>,
        secret_provider: Arc<dyn SecretProvider>,
        bindings: Arc<Mutex<Vec<SecretBinding>>>,
    ) -> Self {
        Self {
            gate: Arc::new(BrokerGate::new(policy_container, secret_provider, bindings)),
        }
    }

    /// Share one gate between the legacy port-54 broker and the port-55 proxy.
    pub fn shared(gate: Arc<BrokerGate>) -> Self {
        Self { gate }
    }

    pub fn gate_handle(&self) -> Arc<BrokerGate> {
        Arc::clone(&self.gate)
    }

    fn modify_gate(&mut self, update: impl FnOnce(&mut BrokerGate)) {
        if let Some(gate) = Arc::get_mut(&mut self.gate) {
            update(gate);
            return;
        }
        tracing::error!("egress gate is already shared; builder update was ignored");
    }

    pub fn with_decision_callback(mut self, cb: EgressDecisionCallback) -> Self {
        self.modify_gate(|gate| gate.decision_callback = Some(cb));
        self
    }

    pub fn with_resolver(mut self, resolver: Arc<dyn HostResolver>) -> Self {
        self.modify_gate(|gate| gate.resolver = resolver);
        self
    }

    pub fn with_passthrough_hosts(mut self, hosts: Vec<String>) -> Self {
        self.modify_gate(|gate| gate.passthrough_hosts = hosts);
        self
    }

    pub fn with_ip_deny_exemption(mut self) -> Self {
        self.modify_gate(|gate| gate.exempt_ip_deny = true);
        self
    }

    pub fn set_max_egress_bytes(&self, max_bytes: u64) {
        self.gate
            .max_egress_bytes
            .store(max_bytes, Ordering::SeqCst);
    }

    pub fn get_total_egress_bytes(&self) -> u64 {
        self.gate.total_egress_bytes.load(Ordering::SeqCst)
    }

    pub fn gate(&self) -> &BrokerGate {
        &self.gate
    }

    pub async fn handle_connection<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::channel::<Frame>(128);

        // Spawn writer loop
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if let Err(e) = frame.write_to(&mut writer).await {
                    eprintln!("EgressBrokerServer writer error: {}", e);
                    break;
                }
            }
        });

        loop {
            match Frame::read_from(&mut reader).await {
                Ok(frame) => {
                    if frame.frame_type != FrameType::StreamEgress {
                        continue;
                    }
                    let envelope = match VsockEnvelope::decode(frame.payload.as_ref()) {
                        Ok(env) => env,
                        Err(e) => {
                            eprintln!("Failed to decode VsockEnvelope: {}", e);
                            continue;
                        }
                    };

                    let response_envelope = self.process_request(envelope).await;
                    let Some(response_frame) =
                        response_frame(frame.correlation_id, response_envelope)
                    else {
                        eprintln!("Failed to encode response envelope");
                        continue;
                    };

                    if tx.send(response_frame).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    if e.kind() != io::ErrorKind::UnexpectedEof {
                        eprintln!("EgressBrokerServer reader error: {}", e);
                    }
                    break;
                }
            }
        }
    }

    async fn process_request(&self, envelope: VsockEnvelope) -> VsockEnvelope {
        let method = envelope.method.as_str();
        let correlation_id = envelope.correlation_id;

        if method != "Egress" {
            return make_error_envelope(
                method,
                correlation_id,
                VamErrorCode::Unimplemented,
                "Unsupported method",
            );
        }

        let req = match EgressRequest::decode(envelope.payload.as_slice()) {
            Ok(r) => r,
            Err(_) => {
                return make_error_envelope(
                    method,
                    correlation_id,
                    VamErrorCode::InvalidArgument,
                    "Invalid request payload",
                )
            }
        };

        let raw_url = req.url.clone();
        let req_bytes_len = (req.url.len() + req.body.len()) as u64;
        match self.handle_egress(req).await {
            Ok(resp) => {
                let resp_bytes_len = resp.body.len() as u64;
                // Phase 5: Record network flow metrics
                let reg = vam_telemetry::VamMetricsRegistry::global();
                let _ = reg.record_counter(
                    vam_telemetry::METRIC_NETWORK_EGRESS_BYTES_TOTAL,
                    req_bytes_len,
                    &[("direction", "egress")],
                );
                let _ = reg.record_counter(
                    vam_telemetry::METRIC_NETWORK_INGRESS_BYTES_TOTAL,
                    resp_bytes_len,
                    &[("direction", "ingress")],
                );
                let _ = reg.record_counter(
                    vam_telemetry::METRIC_NETWORK_PACKETS_TOTAL,
                    1,
                    &[("direction", "egress")],
                );

                make_envelope(method, correlation_id, &resp)
            }
            Err(failure) => {
                let err = &failure.error;
                // Phase 5: Record dropped packets and route deny log to security lane
                let reg = vam_telemetry::VamMetricsRegistry::global();
                let _ = reg.record_counter(
                    vam_telemetry::METRIC_NETWORK_DROPPED_PACKETS_TOTAL,
                    1,
                    &[("result", "blocked")],
                );

                let parsed_url = reqwest::Url::parse(&raw_url).ok();
                let domain = parsed_url
                    .as_ref()
                    .and_then(|url| url.host_str().map(|host| host.to_string()))
                    .unwrap_or_else(|| "unknown".to_string());
                let deny_port = parsed_url
                    .as_ref()
                    .and_then(|url| url.port_or_known_default())
                    .map(u32::from)
                    .unwrap_or(443);

                tracing::warn!(
                    target: "vam_security_network",
                    event = "connection_blocked",
                    destination = %domain,
                    reason = %err,
                    "Security lane: Egress connection blocked by policy"
                );

                if let Some(ref cb) = self.gate.decision_callback {
                    cb(
                        &domain,
                        deny_port,
                        "DENY",
                        &decision_label("active-policy", failure.rule.as_deref().unwrap_or("")),
                    );
                }
                VsockEnvelope {
                    method: method.to_string(),
                    correlation_id,
                    payload: Vec::new(),
                    error: Some(ErrorStatus {
                        code: VamErrorCode::PermissionDenied as i32,
                        message: err.to_string(),
                        retryable: false,
                        details: Vec::new(),
                    }),
                    trace_context: None,
                }
            }
        }
    }

    async fn handle_egress(&self, req: EgressRequest) -> Result<EgressResponse, EgressFailure> {
        let req_len = req.url.len() + req.body.len();
        // Port 54 buffers every response and scrubs it in `finish`, so `secret_bound` (the
        // proxy's oversize rule) does not apply here.
        let PreparedCall {
            req,
            decision,
            rule,
            mut ticket,
            ..
        } = self.gate.prepare_with_decision(req).await?;

        let method = match map_http_method(&req.method) {
            Ok(method) => method,
            Err(()) => return Ok(unsupported_method_response(&req.method)),
        };

        #[allow(unused_mut)]
        let mut url = reqwest::Url::parse(&req.url).map_err(|err| {
            VamNetworkError::Internal(format!("effective URL failed to reparse: {err}"))
        })?;
        let host = url
            .host_str()
            .ok_or_else(|| VamNetworkError::Internal("effective URL lost its host".to_string()))?
            .to_string();
        let host = host.as_str();
        // TEST ONLY: the redirected host is dialled as plain http on its loopback server.
        #[cfg(feature = "process-guest")]
        if let Some(redirect) = self.gate.redirect_for(host) {
            let _ = url.set_scheme("http");
            let _ = url.set_ip_host(redirect.addr.ip());
            let _ = url.set_port(Some(redirect.addr.port()));
            debug_assert_eq!(url.host_str(), redirect.base.host_str());
        }

        let policy_generation = self.gate.policy_container.load().policy_generation;
        eprintln!(
            "[egress:trace] Policy Allowed Dispatching outbound {} to https://{}:{}",
            req.method,
            host,
            decision.addr.port()
        );
        if let Some(ref cb) = self.gate.decision_callback {
            cb(
                host,
                u32::from(decision.addr.port()),
                "ALLOW",
                &decision_label(&format!("gen-{}", policy_generation), &rule),
            );
        }

        let client = reqwest::Client::builder()
            .resolve(host, decision.addr)
            .redirect(reqwest::redirect::Policy::none())
            // Ambient HTTP(S)_PROXY must not bypass the pinned address or the deny set.
            .no_proxy()
            .build()
            .map_err(|err| {
                VamNetworkError::Internal(format!("Failed to build reqwest client: {}", err))
            })?;

        let mut builder = client.request(method, url);
        let sanitized_headers = crate::policy::HeaderFilter::sanitize_headers(req.headers);
        for (key, value) in sanitized_headers {
            builder = builder.header(key, value);
        }
        if !req.body.is_empty() {
            builder = builder.body(req.body);
        }

        // A journaled write is recorded as sent immediately before it leaves the host; a
        // journal that cannot be written sends nothing (R1).
        if let Some(ticket) = ticket.as_mut() {
            ticket.commit_send()?;
        }
        let resp = builder
            .send()
            .await
            .map_err(|err| VamNetworkError::Internal(format!("Outbound HTTP failed: {}", err)))?;

        let status_code = resp.status().as_u16();
        if let Some(ticket) = ticket.as_mut() {
            ticket.answered(status_code);
        }
        eprintln!(
            "[egress:trace] Outbound HTTP Response Status: {}, Body pending",
            status_code
        );
        let mut headers = HashMap::new();
        for (key, value) in resp.headers() {
            if let Ok(val) = value.to_str() {
                headers.insert(key.to_string(), val.to_string());
            }
        }
        let body = resp
            .bytes()
            .await
            .map_err(|err| {
                VamNetworkError::Internal(format!("Failed to read response body: {}", err))
            })?
            .to_vec();

        EgressGate::record_bytes(self.gate.as_ref(), req_len as u64).map_err(gate_err_to_net)?;
        let egress_resp = EgressResponse {
            status_code: u32::from(status_code),
            headers,
            body,
            error: None,
        };
        let mut finished = self
            .gate
            .finish(egress_resp)
            .await
            .map_err(gate_err_to_net)?;
        // The scrub may have rewritten the body, so the upstream framing
        // (Content-Length, Transfer-Encoding) no longer describes it. The
        // guest recomputes the length from the body it receives.
        finished
            .headers
            .retain(|name, _| !crate::proxy::is_hop_by_hop(name) && !is_body_framing(name));
        Ok(finished)
    }
}

fn is_body_framing(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-length")
}

fn map_http_method(method: &str) -> Result<reqwest::Method, ()> {
    Ok(match method.to_ascii_uppercase().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        "OPTIONS" => reqwest::Method::OPTIONS,
        "TRACE" => reqwest::Method::TRACE,
        "CONNECT" => reqwest::Method::CONNECT,
        _ => return Err(()),
    })
}

fn unsupported_method_response(method: &str) -> EgressResponse {
    let message = format!("unsupported HTTP method '{}'", method);
    EgressResponse {
        status_code: 400,
        headers: HashMap::new(),
        body: message.as_bytes().to_vec(),
        error: Some(ErrorStatus {
            code: VamErrorCode::InvalidArgument as i32,
            message,
            retryable: false,
            details: Vec::new(),
        }),
    }
}

fn legacy_channel_oversize_response() -> EgressResponse {
    EgressResponse {
        status_code: 502,
        headers: HashMap::new(),
        body: LEGACY_CHANNEL_OVERSIZE.as_bytes().to_vec(),
        error: Some(ErrorStatus {
            code: VamErrorCode::ResourceExhausted as i32,
            message: LEGACY_CHANNEL_OVERSIZE.to_string(),
            retryable: false,
            details: Vec::new(),
        }),
    }
}

fn response_frame(correlation_id: u32, envelope: VsockEnvelope) -> Option<Frame> {
    let method = envelope.method.clone();
    let env_correlation = envelope.correlation_id;
    let mut env_bytes = Vec::new();
    if envelope.encode(&mut env_bytes).is_err() {
        return None;
    }
    if env_bytes.len() > MAX_PAYLOAD_SIZE {
        let replacement = make_envelope(
            &method,
            env_correlation,
            &legacy_channel_oversize_response(),
        );
        env_bytes.clear();
        if replacement.encode(&mut env_bytes).is_err() || env_bytes.len() > MAX_PAYLOAD_SIZE {
            return None;
        }
    }
    Some(Frame {
        frame_type: FrameType::StreamEgress,
        correlation_id,
        payload: Bytes::from(env_bytes),
    })
}

/// Phase 1 default: terminate TLS on port 443, passthrough on every other
/// allowed port. `passthrough_hosts` forces passthrough so clients that pin
/// certificates can still connect.
fn connect_gate_mode(host: &str, port: u16, passthrough_hosts: &[String]) -> GateMode {
    let key = normalize_passthrough_host(host);
    if port != 443
        || passthrough_hosts
            .iter()
            .any(|candidate| normalize_passthrough_host(candidate) == key)
    {
        GateMode::Passthrough
    } else {
        GateMode::Terminate
    }
}

fn denied_by_rule_message(host: &str, rule: &str) -> String {
    format!("Access to host '{host}' is blocked by active network policy (rule: {rule})")
}

/// Rule recorded when the platform IP floor refuses a resolved address.
const PLATFORM_IP_FLOOR_RULE: &str = "platform ip floor";
/// Rule recorded when the host is allowed but the port is not.
const PORT_ALLOWLIST_RULE: &str = "port allowlist";

/// Host as the authorizer sees it: lower-case, no trailing dot. `x.example.com.`
/// names the same host as `x.example.com`, so it must not slip past a pattern.
fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// `policy_id` on the `egress.decided` event and the OCSF block event: the
/// rule that decided (when an authorizer decided), then the policy generation
/// and mode. The suffix is unchanged so existing readers keep working.
fn decision_label(policy_id: &str, rule: &str) -> String {
    if rule.is_empty() {
        policy_id.to_string()
    } else {
        format!("rule={rule}; {policy_id}")
    }
}

fn normalize_passthrough_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn gate_err_to_net(err: GateError) -> VamNetworkError {
    match err {
        GateError::Denied(reason) | GateError::Resolution(reason) => {
            VamNetworkError::EgressForbidden { reason }
        }
        GateError::QuotaExceeded => VamNetworkError::EgressForbidden {
            reason: "Egress byte quota exceeded".to_string(),
        },
        GateError::Internal(message) => VamNetworkError::Internal(message),
    }
}

fn net_err_to_gate(err: VamNetworkError) -> GateError {
    match err {
        VamNetworkError::EgressForbidden { reason } => {
            if reason.starts_with("Egress byte quota exceeded") {
                GateError::QuotaExceeded
            } else if reason.starts_with("Failed to resolve host")
                || reason.starts_with("No IP addresses found")
            {
                GateError::Resolution(reason)
            } else {
                GateError::Denied(reason)
            }
        }
        other => GateError::Internal(other.to_string()),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UnixListener, UnixStream};
    use vam_spec::v1::{AllowedPortRange, NetworkMode, NetworkPolicy, NetworkPolicySnapshot};

    struct Loopback;

    impl HostResolver for Loopback {
        fn lookup(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            Ok(vec![SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port)])
        }
    }

    struct AllowEveryHost;

    impl EgressAuthorizer for AllowEveryHost {
        fn authorize(&self, _host: &str) -> EgressAuthorization {
            EgressAuthorization::Allow {
                rule: "test".to_string(),
            }
        }
    }

    fn bound_gate() -> BrokerGate {
        let raw_policy = NetworkPolicy {
            mode: NetworkMode::Brokered as i32,
            allowed_domains: vec!["bound.example".to_string(), "free.example".to_string()],
            allowed_ports: vec![AllowedPortRange {
                from_port: 443,
                to_port: 443,
                protocol: "tcp".to_string(),
            }],
            ..Default::default()
        };
        let snap = crate::policy::NetworkPolicyCompiler::compile("sess-b", 1, raw_policy).unwrap();
        let provider = Arc::new(crate::secrets::InMemorySecretProvider::new());
        provider.insert("k", "super-secret-token");
        let bindings = Arc::new(Mutex::new(vec![SecretBinding {
            binding_id: "k".to_string(),
            placeholder: "vamsec_k".to_string(),
            secret_ref: "k".to_string(),
            allowed_destinations: vec!["bound.example".to_string()],
            inject_at: Some(vam_spec::v1::InjectionSite {
                r#type: vam_spec::v1::InjectionSiteType::InjectionSiteHeader as i32,
                key: "Authorization".to_string(),
            }),
        }]));
        BrokerGate::new(
            Arc::new(ActivePolicyContainer::new(snap)),
            provider,
            bindings,
        )
        .with_authorizer(Arc::new(AllowEveryHost))
        .with_resolver(Arc::new(Loopback))
        .with_ip_deny_exemption()
    }

    fn get_request(url: &str, authorization: Option<&str>) -> EgressRequest {
        EgressRequest {
            method: "GET".to_string(),
            url: url.to_string(),
            headers: authorization
                .map(|value| HashMap::from([("Authorization".to_string(), value.to_string())]))
                .unwrap_or_default(),
            body: Vec::new(),
        }
    }

    #[tokio::test]
    async fn secret_bound_is_per_destination_not_per_request() {
        let gate = bound_gate();
        // A host with a binding, a request without a placeholder: still bound.
        let plain = gate
            .prepare_request(get_request("https://bound.example/data", None))
            .await
            .unwrap();
        assert!(plain.secret_bound);
        assert!(plain.request.headers.is_empty(), "nothing was injected");
        // The same host with the placeholder: bound, and the value is substituted.
        let injected = gate
            .prepare_request(get_request(
                "https://bound.example/data",
                Some("Bearer vamsec_k"),
            ))
            .await
            .unwrap();
        assert!(injected.secret_bound);
        assert_eq!(
            injected
                .request
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer super-secret-token")
        );
        // A host with no binding is not bound, however it is asked.
        let free = gate
            .prepare_request(get_request("https://free.example/data", None))
            .await
            .unwrap();
        assert!(!free.secret_bound);
        // A placeholder sent to the wrong host is refused, not passed through.
        assert!(gate
            .prepare_request(get_request(
                "https://free.example/data",
                Some("Bearer vamsec_k"),
            ))
            .await
            .is_err());
        // The provided `prepare` is the same call without the flag.
        let via_prepare = gate
            .prepare(get_request("https://bound.example/data", None))
            .await
            .unwrap();
        assert_eq!(via_prepare.url, "https://bound.example/data");
    }

    #[test]
    fn response_refused_emits_a_deny_decision_with_the_reason_as_the_rule() {
        let calls = Arc::new(Mutex::new(Vec::<(String, u32, String, String)>::new()));
        let seen = Arc::clone(&calls);
        let gate =
            bound_gate().with_decision_callback(Arc::new(move |host, port, disposition, label| {
                seen.lock().unwrap().push((
                    host.to_string(),
                    port,
                    disposition.to_string(),
                    label.to_string(),
                ));
            }));
        gate.response_refused(
            "bound.example",
            443,
            crate::gate::REFUSED_OVERSIZE_WITH_SECRET,
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "bound.example");
        assert_eq!(calls[0].1, 443);
        assert_eq!(calls[0].2, "DENY");
        assert!(
            calls[0].3.contains("rule=oversize_with_secret"),
            "{}",
            calls[0].3
        );
    }

    #[tokio::test]
    async fn test_egress_broker_loopback_allowed_and_blocked() {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        // 1. Setup policy container allowing only anthropic.com
        let raw_policy = NetworkPolicy {
            mode: NetworkMode::Brokered as i32,
            max_egress_bps: 0,
            max_ingress_bps: 0,
            max_packet_rate_pps: 0,
            allowed_domains: vec!["api.anthropic.com".to_string()],
            allowed_cidr_destinations: vec![],
            allowed_ports: vec![],
        };
        let snap = crate::policy::NetworkPolicyCompiler::compile("sess-1", 1, raw_policy).unwrap();
        let container = Arc::new(ActivePolicyContainer::new(snap));

        let secret_provider = Arc::new(crate::secrets::InMemorySecretProvider::new());
        let bindings = Arc::new(Mutex::new(Vec::<SecretBinding>::new()));
        let server = Arc::new(EgressBrokerServer::new(
            container,
            secret_provider,
            bindings,
        ));

        // Spawn server accept task
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                server.handle_connection(stream).await;
            }
        });

        // 2. Setup client
        let client_stream = UnixStream::connect(&sock_path).await.unwrap();
        let client = EgressBrokerClient::new(client_stream);

        // Test request to BLOCKED domain (github.com)
        let blocked_req = EgressRequest {
            method: "GET".to_string(),
            url: "https://api.github.com/zen".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let resp = client.dispatch(&blocked_req).await.unwrap();
        assert!(resp.error.is_some());
        assert_eq!(
            resp.error.unwrap().code,
            VamErrorCode::PermissionDenied as i32
        );

        // Test request to ALLOWED but non-existing/mock domain (will fail DNS resolution gracefully but won't be PermissionDenied)
        let allowed_req = EgressRequest {
            method: "GET".to_string(),
            url: "https://api.anthropic.com/v1/messages".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let resp2 = client.dispatch(&allowed_req).await.unwrap();
        // Since api.anthropic.com is allowed but our test container has no external net, it will report lookup failure
        let err = resp2.error.unwrap();
        assert_eq!(err.code, VamErrorCode::PermissionDenied as i32);
        assert!(
            err.message.contains("Failed to resolve host")
                || err.message.contains("blocked by active network policy")
        );
    }

    #[tokio::test]
    async fn test_egress_broker_byte_quota_exceeded() {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("broker_quota.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let raw_policy = NetworkPolicy {
            mode: NetworkMode::Brokered as i32,
            max_egress_bps: 0,
            max_ingress_bps: 0,
            max_packet_rate_pps: 0,
            allowed_domains: vec!["api.anthropic.com".to_string()],
            allowed_cidr_destinations: vec![],
            allowed_ports: vec![],
        };
        let snap =
            crate::policy::NetworkPolicyCompiler::compile("sess-quota", 1, raw_policy).unwrap();
        let container = Arc::new(ActivePolicyContainer::new(snap));

        let secret_provider = Arc::new(crate::secrets::InMemorySecretProvider::new());
        let bindings = Arc::new(Mutex::new(Vec::<SecretBinding>::new()));
        let server = Arc::new(EgressBrokerServer::new(
            container,
            secret_provider,
            bindings,
        ));
        // Set a small quota of 50 bytes
        server.set_max_egress_bytes(50);

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                server.handle_connection(stream).await;
            }
        });

        let client_stream = UnixStream::connect(&sock_path).await.unwrap();
        let client = EgressBrokerClient::new(client_stream);

        // Request with url + body > 50 bytes
        let large_req = EgressRequest {
            method: "POST".to_string(),
            url: "https://api.anthropic.com/v1/messages".to_string(),
            headers: HashMap::new(),
            body: vec![b'x'; 100],
        };
        let resp = client.dispatch(&large_req).await.unwrap();
        assert!(resp.error.is_some());
        let err = resp.error.unwrap();
        assert!(err.message.contains("Egress byte quota exceeded"));
    }

    struct FixedResolver {
        answers: Vec<(String, Vec<IpAddr>)>,
    }

    impl HostResolver for FixedResolver {
        fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            for (name, ips) in &self.answers {
                if name == host {
                    return Ok(ips
                        .iter()
                        .copied()
                        .map(|ip| SocketAddr::new(ip, port))
                        .collect());
                }
            }
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("host not in FixedResolver: {host}"),
            ))
        }
    }

    fn direct_snapshot(
        domains: Vec<String>,
        ports: Vec<AllowedPortRange>,
    ) -> NetworkPolicySnapshot {
        NetworkPolicySnapshot {
            session_id: "sess-gate".to_string(),
            policy_generation: 3,
            compiled_at_timestamp: 0,
            policy: Some(NetworkPolicy {
                mode: NetworkMode::Brokered as i32,
                max_egress_bps: 0,
                max_ingress_bps: 0,
                max_packet_rate_pps: 0,
                allowed_domains: domains,
                allowed_cidr_destinations: vec![],
                allowed_ports: ports,
            }),
            resolved_allowed_ips: vec![],
        }
    }

    fn server_for(domains: Vec<String>, ports: Vec<AllowedPortRange>) -> EgressBrokerServer {
        let container = Arc::new(ActivePolicyContainer::new(direct_snapshot(domains, ports)));
        EgressBrokerServer::new(
            container,
            Arc::new(crate::secrets::InMemorySecretProvider::new()),
            Arc::new(Mutex::new(Vec::<SecretBinding>::new())),
        )
    }

    fn resolver(host: &str, ips: Vec<IpAddr>) -> Arc<dyn HostResolver> {
        Arc::new(FixedResolver {
            answers: vec![(host.to_string(), ips)],
        })
    }

    async fn connect_broker(
        server: Arc<EgressBrokerServer>,
    ) -> (tempfile::TempDir, EgressBrokerClient) {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                server.handle_connection(stream).await;
            }
        });
        let client_stream = UnixStream::connect(&sock_path).await.unwrap();
        (dir, EgressBrokerClient::new(client_stream))
    }

    async fn spawn_local_http() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut collected = Vec::new();
                    let mut buf = [0u8; 1024];
                    loop {
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        collected.extend_from_slice(&buf[..n]);
                        if collected.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                        if collected.len() > 64 * 1024 {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&collected);
                    let line = text.lines().next().unwrap_or("");
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or("");
                    let path = parts.next().unwrap_or("");
                    let body = if path.starts_with("/big") {
                        vec![b'x'; 80 * 1024]
                    } else {
                        method.as_bytes().to_vec()
                    };
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(header.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });
        addr
    }

    /// Upstream that answers every request with `body` and an accurate
    /// `Content-Length`, like httpbin echoing a URL that holds a secret.
    async fn spawn_fixed_body_http(body: String) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut collected = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !collected.windows(4).any(|window| window == b"\r\n\r\n") {
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        collected.extend_from_slice(&buf[..n]);
                    }
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(header.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                });
            }
        });
        addr
    }

    /// The scrub rewrites the body, so the upstream framing headers
    /// no longer describe it and must not reach the guest.
    async fn assert_scrubbed_response_has_no_stale_framing(secret: &str, placeholder: &str) {
        let body = format!(r#"{{"url": "{secret}/anything", "origin": "203.0.113.9"}}"#);
        let upstream_len = body.len();
        let addr = spawn_fixed_body_http(body).await;

        let provider = Arc::new(crate::secrets::InMemorySecretProvider::new());
        provider.insert("jira/base-url", secret);
        let bindings = Arc::new(Mutex::new(vec![SecretBinding {
            binding_id: "jira/base-url".to_string(),
            placeholder: placeholder.to_string(),
            secret_ref: "jira/base-url".to_string(),
            allowed_destinations: vec!["public.example".to_string()],
            inject_at: Some(vam_spec::v1::InjectionSite {
                r#type: vam_spec::v1::InjectionSiteType::InjectionSiteHeader as i32,
                key: "Authorization".to_string(),
            }),
        }]));
        let container = Arc::new(ActivePolicyContainer::new(direct_snapshot(
            vec!["public.example".to_string()],
            vec![AllowedPortRange {
                from_port: u32::from(addr.port()),
                to_port: u32::from(addr.port()),
                protocol: "tcp".to_string(),
            }],
        )));
        let server = Arc::new(
            EgressBrokerServer::new(container, provider, bindings)
                .with_resolver(resolver("public.example", vec![addr.ip()]))
                .with_ip_deny_exemption(),
        );
        let (_dir, client) = connect_broker(server).await;
        let resp = client
            .dispatch(&EgressRequest {
                method: "GET".to_string(),
                url: format!("http://public.example:{}/anything", addr.port()),
                headers: HashMap::new(),
                body: Vec::new(),
            })
            .await
            .unwrap();
        assert!(resp.error.is_none(), "upstream error: {:?}", resp.error);

        let body = String::from_utf8(resp.body.clone()).unwrap();
        assert!(body.contains(placeholder), "{body}");
        assert!(!body.contains(secret), "{body}");
        assert_ne!(body.len(), upstream_len, "scrub must change the length");
        for name in resp.headers.keys() {
            assert!(
                !name.eq_ignore_ascii_case("content-length")
                    && !name.eq_ignore_ascii_case("transfer-encoding")
                    && !name.eq_ignore_ascii_case("connection")
                    && !name.eq_ignore_ascii_case("keep-alive"),
                "stale framing header {name} survived the scrub: {:?}",
                resp.headers
            );
        }
        assert_eq!(
            resp.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn scrubbed_response_drops_framing_when_placeholder_is_shorter() {
        assert_scrubbed_response_has_no_stale_framing(
            "https://tenant-with-a-long-name.example.net",
            "vamsec_jira",
        )
        .await;
    }

    #[tokio::test]
    async fn scrubbed_response_drops_framing_when_placeholder_is_longer() {
        assert_scrubbed_response_has_no_stale_framing(
            "s3cr3t",
            "vamsec_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .await;
    }

    #[tokio::test]
    async fn test_mixed_public_and_private_resolution_is_denied() {
        let server = server_for(vec!["mixed.example".to_string()], vec![]).with_resolver(Arc::new(
            FixedResolver {
                answers: vec![(
                    "mixed.example".to_string(),
                    vec!["8.8.8.8".parse().unwrap(), "10.0.0.1".parse().unwrap()],
                )],
            },
        ));
        let err = server
            .gate()
            .check_destination("mixed.example", 443, "https")
            .await
            .unwrap_err();
        let GateError::Denied(reason) = err else {
            panic!("expected deny, got {err:?}");
        };
        assert!(
            reason.contains("10.0.0.1"),
            "mixed A record must be denied: {reason}"
        );
    }

    #[tokio::test]
    async fn test_bracketed_ipv6_literal_is_denied_before_dns() {
        // A URL keeps the brackets around an IPv6 literal. The resolver has no
        // answers, so any DNS attempt fails the test with a resolution error.
        let server = server_for(vec!["example.com".to_string()], vec![])
            .with_resolver(Arc::new(FixedResolver { answers: vec![] }));
        for host in [
            "[::ffff:127.0.0.1]",
            "[::ffff:169.254.169.254]",
            "[::ffff:10.0.0.1]",
            "[::1]",
            "::ffff:127.0.0.1",
        ] {
            let err = server
                .gate()
                .check_destination(host, 80, "http")
                .await
                .unwrap_err();
            let GateError::Denied(reason) = err else {
                panic!("{host}: expected deny before DNS, got {err:?}");
            };
            assert!(
                reason.contains("Protected Destination Deny Set"),
                "{host}: expected SSRF deny, got {reason}"
            );
        }
    }

    #[tokio::test]
    async fn test_dns_answer_ipv4_mapped_loopback_is_denied() {
        let server = server_for(vec!["mapped.example".to_string()], vec![]).with_resolver(
            resolver("mapped.example", vec!["::ffff:127.0.0.1".parse().unwrap()]),
        );
        let err = server
            .gate()
            .check_destination("mapped.example", 443, "https")
            .await
            .unwrap_err();
        let GateError::Denied(reason) = err else {
            panic!("expected deny, got {err:?}");
        };
        assert!(
            reason.contains("::ffff:127.0.0.1") || reason.contains("127.0.0.1"),
            "mapped loopback answer must be denied: {reason}"
        );
    }

    #[tokio::test]
    async fn test_check_destination_port_allowlist() {
        let open = server_for(vec!["api.example.com".to_string()], vec![]).with_resolver(resolver(
            "api.example.com",
            vec!["1.1.1.1".parse().unwrap()],
        ));
        let allowed = open
            .gate()
            .check_destination("api.example.com", 443, "https")
            .await
            .unwrap();
        assert_eq!(allowed.addr.port(), 443);
        assert_eq!(allowed.addr.ip(), "1.1.1.1".parse::<IpAddr>().unwrap());
        assert!(open
            .gate()
            .check_destination("api.example.com", 22, "https")
            .await
            .is_err());

        let custom = server_for(
            vec!["api.example.com".to_string()],
            vec![AllowedPortRange {
                from_port: 8443,
                to_port: 8443,
                protocol: "tcp".to_string(),
            }],
        )
        .with_resolver(resolver(
            "api.example.com",
            vec!["1.1.1.1".parse().unwrap()],
        ));
        assert!(custom
            .gate()
            .check_destination("api.example.com", 443, "https")
            .await
            .is_err());
        assert!(custom
            .gate()
            .check_destination("api.example.com", 8443, "https")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn connect_mode_terminates_443_unless_host_is_pinned() {
        let names = Arc::new(FixedResolver {
            answers: vec![(
                "api.example.com".to_string(),
                vec!["1.1.1.1".parse().unwrap()],
            )],
        }) as Arc<dyn HostResolver>;
        let open = server_for(vec!["api.example.com".to_string()], vec![]).with_resolver(names);
        let https = open
            .gate()
            .check_destination("api.example.com", 443, "https")
            .await
            .unwrap();
        assert_eq!(https.mode, GateMode::Terminate);
        let connect_443 = open
            .gate()
            .check_destination("api.example.com", 443, "connect")
            .await
            .unwrap();
        assert_eq!(connect_443.mode, GateMode::Terminate);
        let connect_80 = open
            .gate()
            .check_destination("api.example.com", 80, "connect")
            .await
            .unwrap();
        assert_eq!(connect_80.mode, GateMode::Passthrough);

        let pinned = server_for(vec!["api.example.com".to_string()], vec![])
            .with_resolver(resolver(
                "api.example.com",
                vec!["1.1.1.1".parse().unwrap()],
            ))
            .with_passthrough_hosts(vec!["API.example.com.".to_string()]);
        let forced = pinned
            .gate()
            .check_destination("api.example.com", 443, "connect")
            .await
            .unwrap();
        assert_eq!(forced.mode, GateMode::Passthrough);
    }

    #[tokio::test]
    async fn proxy_check_callback_records_real_port_and_mode() {
        let seen = Arc::new(Mutex::new(Vec::<(String, u32, String, String)>::new()));
        let seen_cb = Arc::clone(&seen);
        let server = server_for(vec!["api.example.com".to_string()], vec![])
            .with_resolver(Arc::new(FixedResolver {
                answers: vec![
                    (
                        "api.example.com".to_string(),
                        vec!["1.1.1.1".parse().unwrap()],
                    ),
                    (
                        "denied.example".to_string(),
                        vec!["8.8.8.8".parse().unwrap()],
                    ),
                ],
            }))
            .with_decision_callback(Arc::new(move |domain, port, disposition, policy| {
                seen_cb.lock().unwrap().push((
                    domain.to_string(),
                    port,
                    disposition.to_string(),
                    policy.to_string(),
                ));
            }));
        let allowed = server
            .gate()
            .check_destination("api.example.com", 443, "connect")
            .await
            .unwrap();
        assert_eq!(allowed.mode, GateMode::Terminate);
        let denied = server
            .gate()
            .check_destination("denied.example", 8080, "connect")
            .await;
        assert!(denied.is_err());
        let calls = seen.lock().unwrap().clone();
        assert!(
            calls.iter().any(|(domain, port, disposition, policy)| {
                domain == "api.example.com"
                    && *port == 443
                    && disposition == "ALLOW"
                    && policy.ends_with(":terminate")
            }),
            "allow callback must carry port 443 and terminate, got {calls:?}"
        );
        assert!(
            calls.iter().any(|(domain, port, disposition, policy)| {
                domain == "denied.example"
                    && *port == 8080
                    && disposition == "DENY"
                    && policy.ends_with(":passthrough")
            }),
            "deny callback must carry the real port and passthrough, got {calls:?}"
        );
    }

    #[tokio::test]
    async fn test_decision_callback_deny_uses_real_port() {
        let seen = Arc::new(Mutex::new(Vec::<(String, u32, String)>::new()));
        let seen_cb = Arc::clone(&seen);
        let server = server_for(vec!["other.example".to_string()], vec![])
            .with_resolver(resolver("denied.example", vec!["8.8.8.8".parse().unwrap()]))
            .with_decision_callback(Arc::new(move |domain, port, disposition, _policy| {
                seen_cb
                    .lock()
                    .unwrap()
                    .push((domain.to_string(), port, disposition.to_string()));
            }));
        let server = Arc::new(server);
        let (_dir, client) = connect_broker(Arc::clone(&server)).await;
        let resp = client
            .dispatch(&EgressRequest {
                method: "GET".to_string(),
                url: "https://denied.example:8443/secret".to_string(),
                headers: HashMap::new(),
                body: Vec::new(),
            })
            .await
            .unwrap();
        assert!(resp.error.is_some());
        let calls = seen.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .any(|(domain, port, disposition)| domain == "denied.example"
                    && *port == 8443
                    && disposition == "DENY"),
            "deny callback must carry the real port, got {calls:?}"
        );
        assert!(
            calls.iter().all(|(_, port, _)| *port == 8443),
            "callback must not invent port 443, got {calls:?}"
        );
    }

    #[tokio::test]
    async fn test_unknown_method_is_400() {
        let server = Arc::new(
            server_for(vec!["api.example.com".to_string()], vec![]).with_resolver(resolver(
                "api.example.com",
                vec!["1.1.1.1".parse().unwrap()],
            )),
        );
        let (_dir, client) = connect_broker(server).await;
        let resp = client
            .dispatch(&EgressRequest {
                method: "FOOBAR".to_string(),
                url: "https://api.example.com/v1".to_string(),
                headers: HashMap::new(),
                body: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(resp.status_code, 400);
        let err = resp.error.expect("unknown method returns an error");
        assert_eq!(err.code, VamErrorCode::InvalidArgument as i32);
        assert!(err.message.contains("FOOBAR"));
    }

    #[tokio::test]
    async fn test_patch_reaches_local_upstream_and_reports_port() {
        let addr = spawn_local_http().await;
        let seen = Arc::new(Mutex::new(Vec::<(String, u32, String)>::new()));
        let seen_cb = Arc::clone(&seen);
        let server = Arc::new(
            server_for(
                vec!["public.example".to_string()],
                vec![AllowedPortRange {
                    from_port: u32::from(addr.port()),
                    to_port: u32::from(addr.port()),
                    protocol: "tcp".to_string(),
                }],
            )
            .with_resolver(resolver("public.example", vec![addr.ip()]))
            .with_ip_deny_exemption()
            .with_decision_callback(Arc::new(
                move |domain, port, disposition, _policy| {
                    seen_cb.lock().unwrap().push((
                        domain.to_string(),
                        port,
                        disposition.to_string(),
                    ));
                },
            )),
        );
        let (_dir, client) = connect_broker(Arc::clone(&server)).await;
        let resp = client
            .dispatch(&EgressRequest {
                method: "PATCH".to_string(),
                url: format!("http://public.example:{}/echo", addr.port()),
                headers: HashMap::new(),
                body: Vec::new(),
            })
            .await
            .unwrap();
        assert!(resp.error.is_none(), "upstream error: {:?}", resp.error);
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.body, b"PATCH");
        let calls = seen.lock().unwrap().clone();
        assert!(
            calls.iter().any(|(domain, port, disposition)| {
                domain == "public.example"
                    && *port == u32::from(addr.port())
                    && disposition == "ALLOW"
            }),
            "allow callback must carry the real port, got {calls:?}"
        );
        assert!(server.get_total_egress_bytes() > 0);
    }

    #[test]
    fn record_bytes_refuses_past_cap_and_zero_is_unlimited() {
        let snap = direct_snapshot(vec!["public.example".to_string()], vec![]);
        let capped = BrokerGate::new(
            Arc::new(ActivePolicyContainer::new(snap.clone())),
            Arc::new(crate::secrets::InMemorySecretProvider::new()),
            Arc::new(Mutex::new(Vec::<SecretBinding>::new())),
        )
        .with_max_egress_bytes(100);
        assert!(EgressGate::record_bytes(&capped, 60).is_ok());
        assert!(matches!(
            EgressGate::record_bytes(&capped, 41),
            Err(GateError::QuotaExceeded)
        ));
        // The refused chunk was not counted, so a smaller one still fits.
        assert!(EgressGate::record_bytes(&capped, 40).is_ok());
        assert!(matches!(
            EgressGate::record_bytes(&capped, 1),
            Err(GateError::QuotaExceeded)
        ));

        let unlimited = BrokerGate::new(
            Arc::new(ActivePolicyContainer::new(snap)),
            Arc::new(crate::secrets::InMemorySecretProvider::new()),
            Arc::new(Mutex::new(Vec::<SecretBinding>::new())),
        )
        .with_max_egress_bytes(0);
        assert!(EgressGate::record_bytes(&unlimited, u64::from(u32::MAX)).is_ok());
        assert!(EgressGate::record_bytes(&unlimited, u64::from(u32::MAX)).is_ok());
    }

    #[tokio::test]
    async fn connect_tunnel_closes_at_real_gate_byte_cap() {
        // Echo upstream: every byte the guest sends comes back, so both
        // directions count against the cap.
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((sock, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let (mut reader, mut writer) = sock.into_split();
                    let _ = tokio::io::copy(&mut reader, &mut writer).await;
                });
            }
        });

        let quota: u64 = 32 * 1024;
        let gate = Arc::new(
            BrokerGate::new(
                Arc::new(ActivePolicyContainer::new(direct_snapshot(
                    vec!["pinned.example".to_string()],
                    vec![AllowedPortRange {
                        from_port: u32::from(echo_addr.port()),
                        to_port: u32::from(echo_addr.port()),
                        protocol: "tcp".to_string(),
                    }],
                ))),
                Arc::new(crate::secrets::InMemorySecretProvider::new()),
                Arc::new(Mutex::new(Vec::<SecretBinding>::new())),
            )
            .with_resolver(resolver("pinned.example", vec![echo_addr.ip()]))
            .with_ip_deny_exemption()
            .with_max_egress_bytes(quota),
        );

        let (mut client, proxy_side) = tokio::io::duplex(8 * 1024);
        let proxy_gate: Arc<dyn EgressGate> = Arc::clone(&gate) as Arc<dyn EgressGate>;
        tokio::spawn(async move {
            let _ = crate::proxy::serve_proxy(proxy_side, proxy_gate, None).await;
        });

        let target = format!("pinned.example:{}", echo_addr.port());
        client
            .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let byte = client
                .read_u8()
                .await
                .expect("proxy closed before answering CONNECT");
            head.push(byte);
        }
        let head = String::from_utf8_lossy(&head);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");

        let payload = vec![0x5Au8; 256 * 1024];
        let (mut reader, mut writer) = tokio::io::split(client);
        let sending = payload.clone();
        let write = async move {
            let _ = writer.write_all(&sending).await;
            let _ = writer.shutdown().await;
        };
        let read = async move {
            let mut got = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                match reader.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&tmp[..n]),
                }
            }
            got
        };
        let ((), got) =
            tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(write, read) })
                .await
                .expect("capped tunnel never closed");

        assert!(
            (got.len() as u64) < payload.len() as u64,
            "cap did not close the tunnel (echoed {} of {})",
            got.len(),
            payload.len()
        );
        assert!(gate.total_egress_bytes.load(Ordering::SeqCst) <= quota);
    }

    #[tokio::test]
    async fn test_oversize_response_is_an_error_and_channel_stays_up() {
        let addr = spawn_local_http().await;
        let server = Arc::new(
            server_for(
                vec!["public.example".to_string()],
                vec![AllowedPortRange {
                    from_port: u32::from(addr.port()),
                    to_port: u32::from(addr.port()),
                    protocol: "tcp".to_string(),
                }],
            )
            .with_resolver(resolver("public.example", vec![addr.ip()]))
            .with_ip_deny_exemption(),
        );
        let (_dir, client) = connect_broker(server).await;
        let oversized = client
            .dispatch(&EgressRequest {
                method: "GET".to_string(),
                url: format!("http://public.example:{}/big", addr.port()),
                headers: HashMap::new(),
                body: Vec::new(),
            })
            .await
            .unwrap();
        let err = oversized
            .error
            .expect("over-cap response must be an error, not a dead channel");
        assert_eq!(err.message, LEGACY_CHANNEL_OVERSIZE);

        let follow_up = client
            .dispatch(&EgressRequest {
                method: "GET".to_string(),
                url: format!("http://public.example:{}/small", addr.port()),
                headers: HashMap::new(),
                body: Vec::new(),
            })
            .await
            .expect("channel must still answer after an over-cap response");
        assert!(
            follow_up.error.is_none(),
            "follow-up error: {:?}",
            follow_up.error
        );
        assert_eq!(follow_up.status_code, 200);
        assert_eq!(follow_up.body, b"GET");
    }

    /// Allows every host except `denied`, records what it was asked.
    struct FakeAuthorizer {
        denied: Vec<String>,
        asked: Mutex<Vec<String>>,
    }

    impl FakeAuthorizer {
        fn new(denied: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                denied: denied.iter().map(|h| h.to_string()).collect(),
                asked: Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl EgressAuthorizer for FakeAuthorizer {
        fn authorize(&self, host: &str) -> EgressAuthorization {
            self.asked.lock().unwrap().push(host.to_string());
            if self.denied.iter().any(|d| d == host) {
                EgressAuthorization::Deny {
                    rule: format!("fake deny {host}"),
                }
            } else {
                EgressAuthorization::Allow {
                    rule: "fake allow".to_string(),
                }
            }
        }
    }

    type Seen = Arc<Mutex<Vec<(String, u32, String, String)>>>;

    fn authorized_gate(
        authorizer: Arc<FakeAuthorizer>,
        host: &str,
        ip: &str,
    ) -> (BrokerGate, Seen) {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        // Empty `allowed_domains`: the snapshot allowlist would refuse everything.
        let container = Arc::new(ActivePolicyContainer::new(direct_snapshot(vec![], vec![])));
        let gate = BrokerGate::new(
            container,
            Arc::new(crate::secrets::InMemorySecretProvider::new()),
            Arc::new(Mutex::new(Vec::<SecretBinding>::new())),
        )
        .with_resolver(resolver(host, vec![ip.parse().unwrap()]))
        .with_authorizer(authorizer)
        .with_decision_callback(Arc::new(move |h, p, d, id| {
            sink.lock()
                .unwrap()
                .push((h.to_string(), p, d.to_string(), id.to_string()));
        }));
        (gate, seen)
    }

    #[tokio::test]
    async fn authorizer_replaces_the_snapshot_host_allowlist() {
        let authorizer = FakeAuthorizer::new(&["paste.pastebin.com"]);
        let (gate, seen) = authorized_gate(authorizer, "example.com", "93.184.216.34");
        let decision = gate
            .check_destination("example.com", 443, "https")
            .await
            .unwrap();
        assert_eq!(decision.addr.port(), 443);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].2, "ALLOW");
        assert!(seen[0].3.starts_with("rule=fake allow; "), "{:?}", seen[0]);
    }

    #[tokio::test]
    async fn authorizer_deny_is_refused_and_names_the_rule() {
        let authorizer = FakeAuthorizer::new(&["paste.pastebin.com"]);
        let (gate, seen) = authorized_gate(authorizer, "paste.pastebin.com", "104.20.0.1");
        let err = gate
            .check_destination("paste.pastebin.com", 443, "https")
            .await
            .unwrap_err();
        let GateError::Denied(reason) = err else {
            panic!("expected deny, got {err:?}");
        };
        assert!(reason.contains("fake deny paste.pastebin.com"), "{reason}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].2, "DENY");
        assert!(
            seen[0].3.starts_with("rule=fake deny paste.pastebin.com; "),
            "{:?}",
            seen[0]
        );
    }

    #[tokio::test]
    async fn platform_ip_floor_runs_before_and_without_the_authorizer() {
        // The authorizer allows everything, including metadata and private ranges.
        for ip in ["169.254.169.254", "10.1.2.3", "127.0.0.1", "100.64.0.1"] {
            let authorizer = FakeAuthorizer::new(&[]);
            let (gate, seen) = authorized_gate(Arc::clone(&authorizer), "anywhere.example", ip);
            let err = gate
                .check_destination("anywhere.example", 443, "https")
                .await
                .unwrap_err();
            let GateError::Denied(reason) = err else {
                panic!("{ip}: expected deny, got {err:?}");
            };
            assert!(
                reason.contains("Protected Destination Deny Set"),
                "{ip}: {reason}"
            );
            assert!(authorizer.asked().is_empty(), "{ip}: floor must run first");
            assert!(seen.lock().unwrap()[0]
                .3
                .starts_with("rule=platform ip floor; "));
        }
    }

    #[tokio::test]
    async fn authorizer_sees_a_normalized_host() {
        let authorizer = FakeAuthorizer::new(&["paste.pastebin.com"]);
        let (gate, _seen) =
            authorized_gate(Arc::clone(&authorizer), "Paste.Pastebin.COM.", "104.20.0.1");
        // A trailing dot or upper case must not step around a deny rule.
        assert!(gate
            .check_destination("Paste.Pastebin.COM.", 443, "https")
            .await
            .is_err());
        assert_eq!(authorizer.asked(), vec!["paste.pastebin.com".to_string()]);
    }

    #[tokio::test]
    async fn port_allowlist_still_applies_after_an_authorizer_allow() {
        let authorizer = FakeAuthorizer::new(&[]);
        let (gate, seen) = authorized_gate(authorizer, "example.com", "93.184.216.34");
        let err = gate
            .check_destination("example.com", 22, "https")
            .await
            .unwrap_err();
        assert!(matches!(err, GateError::Denied(_)));
        assert!(seen.lock().unwrap()[0]
            .3
            .starts_with("rule=port allowlist; "));
    }

    #[tokio::test]
    async fn port_54_denial_is_reported_once_with_the_rule() {
        let authorizer = FakeAuthorizer::new(&["paste.pastebin.com"]);
        let (gate, seen) = authorized_gate(authorizer, "paste.pastebin.com", "104.20.0.1");
        let server = EgressBrokerServer::shared(Arc::new(gate));
        let request = EgressRequest {
            method: "GET".to_string(),
            url: "https://paste.pastebin.com/raw/x".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let response = server
            .process_request(make_envelope("Egress", 7, &request))
            .await;
        let error = response.error.expect("denied");
        assert_eq!(error.code, VamErrorCode::PermissionDenied as i32);
        assert!(error.message.contains("fake deny paste.pastebin.com"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one decision event: {seen:?}");
        assert_eq!(seen[0].2, "DENY");
        assert!(seen[0]
            .3
            .starts_with("rule=fake deny paste.pastebin.com; active-policy"));
    }

    #[tokio::test]
    async fn unresolvable_host_denied_by_a_rule_reports_the_rule() {
        let authorizer = FakeAuthorizer::new(&["paste.pastebin.com"]);
        // The resolver knows only example.com, so the denied host fails DNS.
        let (gate, seen) = authorized_gate(authorizer, "example.com", "93.184.216.34");
        let err = gate
            .check_destination("paste.pastebin.com", 443, "https")
            .await
            .unwrap_err();
        assert!(matches!(err, GateError::Denied(_)), "{err:?}");
        assert!(seen.lock().unwrap()[0]
            .3
            .starts_with("rule=fake deny paste.pastebin.com; "));
        // An allowed host that fails DNS stays a resolution failure.
        let err = gate
            .check_destination("nowhere.example", 443, "https")
            .await
            .unwrap_err();
        assert!(matches!(err, GateError::Resolution(_)), "{err:?}");
    }

    // ---- connection bindings (CN-2.2, CN-2.3) ------------------------------------------------

    mod connection_gate {
        use super::*;
        use crate::connection::{
            set_bearer, ActiveCall, BindingSpec, CallWindow, ConnectionRefusal, Envelope, Shape,
            WriteTicket,
        };
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        use std::time::Duration;

        const PH: &str = "vamsec_m365_graph";
        const URN: &str = "urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365";
        const TOKEN: &str = "tok-0123456789abcdef-current";

        /// A real envelope with a counted `attach`, so a test can show it never ran.
        struct CountingSource {
            envelope: Envelope,
            attached: AtomicUsize,
            clear_between: bool,
            /// What happened, in order: `reserve`, `attach`, then the ticket's own lines.
            log: Arc<Mutex<Vec<String>>>,
            /// The journal cannot be written: a write is refused at the reservation.
            refuse_reserve: AtomicBool,
        }

        /// A journal ticket that only records what the gate did with it.
        struct LogTicket(Arc<Mutex<Vec<String>>>);

        impl WriteTicket for LogTicket {
            fn commit_send(&mut self) -> Result<(), ConnectionRefusal> {
                self.0.lock().unwrap().push("commit".into());
                Ok(())
            }

            fn answered(&mut self, status: u16) {
                self.0.lock().unwrap().push(format!("answered:{status}"));
            }
        }

        #[async_trait]
        impl ConnectionTokenSource for CountingSource {
            fn authorize(
                &self,
                request: &ConnectionRequest<'_>,
            ) -> Result<Option<ConnectionGrant>, ConnectionRefusal> {
                let grant = self.envelope.screen(request)?;
                if self.clear_between {
                    // The next intent lands between authorize and attach.
                    self.envelope.window().clear();
                }
                Ok(grant)
            }

            async fn attach(
                &self,
                grant: &ConnectionGrant,
                headers: &mut HashMap<String, String>,
            ) -> Result<(), ConnectionRefusal> {
                self.attached.fetch_add(1, Ordering::SeqCst);
                self.log.lock().unwrap().push("attach".into());
                self.envelope.recheck(grant)?;
                set_bearer(headers, TOKEN);
                Ok(())
            }

            async fn reserve_write(
                &self,
                grant: &ConnectionGrant,
            ) -> Result<Option<Box<dyn WriteTicket>>, ConnectionRefusal> {
                if crate::connection::is_read_method(&grant.method) {
                    return Ok(None);
                }
                self.log.lock().unwrap().push("reserve".into());
                if self.refuse_reserve.load(Ordering::SeqCst) {
                    return Err(ConnectionRefusal {
                        text: "effect_journal_unavailable: not sent".into(),
                        rule: "tool:t effect journal".into(),
                    });
                }
                Ok(Some(Box::new(LogTicket(Arc::clone(&self.log)))))
            }

            fn scrub(&self, response: &mut EgressResponse) {
                crate::connection::scrub_secret(response, TOKEN, PH);
            }

            fn binds_host(&self, host: &str) -> bool {
                self.envelope.binds_host(host)
            }
        }

        struct CountingResolver(AtomicUsize);

        impl HostResolver for CountingResolver {
            fn lookup(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(vec![SocketAddr::new(
                    "93.184.216.34".parse().unwrap(),
                    port,
                )])
            }
        }

        fn read_shape() -> Shape {
            Shape {
                binding: "graph".into(),
                host: "graph.microsoft.com".into(),
                methods: vec!["GET".into()],
                path_prefixes: vec!["/v1.0/me/messages".into()],
                paths: vec![],
            }
        }

        fn read_call() -> ActiveCall {
            ActiveCall::new(
                "call-1",
                "m365_probe_search",
                Some(URN.into()),
                vec![read_shape()],
                Duration::from_secs(60),
            )
        }

        struct Rig {
            gate: BrokerGate,
            source: Arc<CountingSource>,
            lookups: Arc<CountingResolver>,
            seen: Seen,
        }

        fn rig(call: Option<ActiveCall>, denied: &[&str], clear_between: bool) -> Rig {
            let window = Arc::new(CallWindow::new());
            if let Some(c) = call {
                window.open(c);
            }
            let source = Arc::new(CountingSource {
                envelope: Envelope::new(
                    vec![BindingSpec {
                        connector: URN.into(),
                        name: "graph".into(),
                        placeholder: PH.into(),
                        hosts: vec!["graph.microsoft.com".into()],
                    }],
                    window,
                ),
                attached: AtomicUsize::new(0),
                clear_between,
                log: Arc::new(Mutex::new(Vec::new())),
                refuse_reserve: AtomicBool::new(false),
            });
            let seen: Seen = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&seen);
            let lookups = Arc::new(CountingResolver(AtomicUsize::new(0)));
            let container = Arc::new(ActivePolicyContainer::new(direct_snapshot(vec![], vec![])));
            let gate = BrokerGate::new(
                container,
                Arc::new(crate::secrets::InMemorySecretProvider::new()),
                Arc::new(Mutex::new(Vec::<SecretBinding>::new())),
            )
            .with_resolver(Arc::clone(&lookups) as Arc<dyn HostResolver>)
            .with_authorizer(FakeAuthorizer::new(denied))
            .with_connection_tokens(Arc::clone(&source) as Arc<dyn ConnectionTokenSource>)
            .with_decision_callback(Arc::new(move |h, p, d, id| {
                sink.lock()
                    .unwrap()
                    .push((h.to_string(), p, d.to_string(), id.to_string()));
            }));
            Rig {
                gate,
                source,
                lookups,
                seen,
            }
        }

        fn request(method: &str, url: &str, auth: &str, body: &[u8]) -> EgressRequest {
            EgressRequest {
                method: method.into(),
                url: url.into(),
                headers: HashMap::from([("Authorization".to_string(), auth.to_string())]),
                body: body.to_vec(),
            }
        }

        const MSGS: &str = "https://graph.microsoft.com/v1.0/me/messages";

        fn send_call() -> ActiveCall {
            ActiveCall::new(
                "call-1",
                "mail_send",
                Some(URN.into()),
                vec![Shape {
                    binding: "graph".into(),
                    host: "graph.microsoft.com".into(),
                    methods: vec!["POST".into()],
                    path_prefixes: vec![],
                    paths: vec!["/v1.0/me/sendMail".into()],
                }],
                Duration::from_secs(60),
            )
        }

        const SEND: &str = "https://graph.microsoft.com/v1.0/me/sendMail";

        #[tokio::test]
        async fn a_write_is_reserved_before_the_token_is_read_and_handed_to_the_sender() {
            let r = rig(Some(send_call()), &[], false);
            let PreparedEgress {
                request: out,
                ticket,
                secret_bound,
            } = r
                .gate
                .prepare_request(request("POST", SEND, PH, b"{}"))
                .await
                .unwrap();
            assert!(
                secret_bound,
                "a write that gets the token is secret-bound too"
            );
            assert_eq!(
                out.headers.get("Authorization").map(String::as_str),
                Some(format!("Bearer {TOKEN}").as_str())
            );
            let mut ticket = ticket.expect("a write comes with its journal ticket");
            assert_eq!(*r.source.log.lock().unwrap(), vec!["reserve", "attach"]);
            // The sender commits right before it dispatches and reports the status.
            ticket.commit_send().unwrap();
            ticket.answered(202);
            assert_eq!(
                *r.source.log.lock().unwrap(),
                vec!["reserve", "attach", "commit", "answered:202"]
            );
        }

        #[tokio::test]
        async fn a_journal_that_cannot_be_written_refuses_the_write_before_any_token_is_read() {
            let r = rig(Some(send_call()), &[], false);
            r.source.refuse_reserve.store(true, Ordering::SeqCst);
            let err = r
                .gate
                .prepare_request(request("POST", SEND, PH, b"{}"))
                .await
                .expect_err("refused");
            assert!(
                err.to_string().contains("effect_journal_unavailable"),
                "{err}"
            );
            assert_eq!(r.source.attached.load(Ordering::SeqCst), 0);
            assert_eq!(*r.source.log.lock().unwrap(), vec!["reserve"]);
        }

        #[tokio::test]
        async fn a_caller_that_cannot_commit_a_journal_ticket_is_not_handed_a_write() {
            let r = rig(Some(send_call()), &[], false);
            let err = r
                .gate
                .prepare(request("POST", SEND, PH, b"{}"))
                .await
                .expect_err("a plain prepare cannot carry a journaled write");
            assert!(
                err.to_string().contains("commits it before sending"),
                "{err}"
            );
        }

        #[tokio::test]
        async fn a_read_has_no_ticket() {
            let r = rig(Some(read_call()), &[], false);
            let prepared = r
                .gate
                .prepare_request(request("GET", MSGS, PH, b""))
                .await
                .unwrap();
            assert!(prepared.ticket.is_none());
            assert_eq!(*r.source.log.lock().unwrap(), vec!["attach"]);
        }

        #[tokio::test]
        async fn the_declared_read_gets_the_token_after_the_destination_check() {
            let r = rig(Some(read_call()), &[], false);
            let out = r.gate.prepare(request("GET", MSGS, PH, b"")).await.unwrap();
            assert_eq!(
                out.headers.get("Authorization").map(String::as_str),
                Some(format!("Bearer {TOKEN}").as_str())
            );
            assert_eq!(out.headers.len(), 1);
            assert_eq!(r.source.attached.load(Ordering::SeqCst), 1);
            assert_eq!(r.lookups.0.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn a_connection_bound_destination_is_secret_bound_with_or_without_a_placeholder() {
            // The 8 MiB fail-closed rule must cover connection tokens as well as static
            // bindings: `ConnectionTokenSource::scrub` cannot reach a body the proxy won't buffer.
            let r = rig(Some(read_call()), &[], false);
            let granted = r
                .gate
                .prepare_request(request("GET", MSGS, PH, b""))
                .await
                .unwrap();
            assert!(granted.secret_bound, "a request that gets the token");
            let echoed = r
                .gate
                .prepare_request(request("GET", MSGS, "Bearer not-a-placeholder", b""))
                .await
                .unwrap();
            assert!(
                echoed.secret_bound,
                "a request with no placeholder to a host the binding is pinned to"
            );
            assert_eq!(r.source.attached.load(Ordering::SeqCst), 1);
            let free = r
                .gate
                .prepare_request(request("GET", "https://free.example/data", "Bearer x", b""))
                .await
                .unwrap();
            assert!(!free.secret_bound, "a host no binding names");
        }

        #[tokio::test]
        async fn the_mitm_shape_of_the_same_request_is_admitted_the_same_way() {
            // Port 55 hands `prepare` `https://host:443/path` and lower-case header names.
            let r = rig(Some(read_call()), &[], false);
            let mut req = request(
                "GET",
                "https://graph.microsoft.com:443/v1.0/me/messages",
                PH,
                b"",
            );
            req.headers = HashMap::from([("authorization".to_string(), format!("Bearer {PH}"))]);
            let out = r.gate.prepare(req).await.unwrap();
            assert_eq!(out.headers.len(), 1);
            assert_eq!(
                out.headers.get("Authorization").map(String::as_str),
                Some(format!("Bearer {TOKEN}").as_str())
            );
        }

        #[tokio::test]
        async fn every_refusal_happens_before_dns_and_before_attach() {
            let other_call = |tool: &str, urn: Option<&str>, shapes: Vec<Shape>| {
                Some(ActiveCall::new(
                    "call-x",
                    tool,
                    urn.map(str::to_string),
                    shapes,
                    Duration::from_secs(60),
                ))
            };
            let mut other_host = read_shape();
            other_host.host = "evil.example".into();
            let mut other_binding = read_shape();
            other_binding.binding = "other".into();
            let cases: Vec<(&str, Option<ActiveCall>, EgressRequest)> = vec![
                ("no window", None, request("GET", MSGS, PH, b"")),
                (
                    "undeclared tool",
                    other_call("bare", Some(URN), vec![]),
                    request("GET", MSGS, PH, b""),
                ),
                (
                    "no connector",
                    other_call("t", None, vec![read_shape()]),
                    request("GET", MSGS, PH, b""),
                ),
                (
                    "other urn",
                    other_call(
                        "t",
                        Some("urn:vam:connector:evil.example/x/microsoft-365"),
                        vec![read_shape()],
                    ),
                    request("GET", MSGS, PH, b""),
                ),
                (
                    "other binding",
                    other_call("t", Some(URN), vec![other_binding]),
                    request("GET", MSGS, PH, b""),
                ),
                (
                    "other host in shape",
                    other_call("t", Some(URN), vec![other_host]),
                    request("GET", MSGS, PH, b""),
                ),
                (
                    "method",
                    Some(read_call()),
                    request(
                        "POST",
                        "https://graph.microsoft.com/v1.0/me/sendMail",
                        PH,
                        b"{}",
                    ),
                ),
                (
                    "path",
                    Some(read_call()),
                    request("GET", "https://graph.microsoft.com/v1.0/users", PH, b""),
                ),
                (
                    "prefix boundary",
                    Some(read_call()),
                    request(
                        "GET",
                        "https://graph.microsoft.com/v1.0/me/messagesX",
                        PH,
                        b"",
                    ),
                ),
                (
                    "dot dot",
                    Some(read_call()),
                    request(
                        "GET",
                        "https://graph.microsoft.com/v1.0/me/messages/../sendMail",
                        PH,
                        b"",
                    ),
                ),
                (
                    "encoded dot dot",
                    Some(read_call()),
                    request(
                        "GET",
                        "https://graph.microsoft.com/v1.0/me/messages/%2e%2e/sendMail",
                        PH,
                        b"",
                    ),
                ),
                (
                    "host",
                    Some(read_call()),
                    request("GET", "https://evil.example/v1.0/me/messages", PH, b""),
                ),
                (
                    "subdomain",
                    Some(read_call()),
                    request(
                        "GET",
                        "https://x.graph.microsoft.com/v1.0/me/messages",
                        PH,
                        b"",
                    ),
                ),
                (
                    "port",
                    Some(read_call()),
                    request(
                        "GET",
                        "https://graph.microsoft.com:8443/v1.0/me/messages",
                        PH,
                        b"",
                    ),
                ),
                (
                    "userinfo",
                    Some(read_call()),
                    request(
                        "GET",
                        "https://u@graph.microsoft.com/v1.0/me/messages",
                        PH,
                        b"",
                    ),
                ),
                (
                    "plain http",
                    Some(read_call()),
                    request(
                        "GET",
                        "http://graph.microsoft.com/v1.0/me/messages",
                        PH,
                        b"",
                    ),
                ),
                (
                    "placeholder in url",
                    Some(read_call()),
                    request("GET", &format!("{MSGS}?t={PH}"), PH, b""),
                ),
                (
                    "placeholder in body",
                    Some(read_call()),
                    request("GET", MSGS, PH, PH.as_bytes()),
                ),
                (
                    "not an exact Authorization value",
                    Some(read_call()),
                    request("GET", MSGS, &format!("Bearer {PH}x"), b""),
                ),
                ("placeholder in another header", Some(read_call()), {
                    let mut q = request("GET", MSGS, PH, b"");
                    q.headers.insert("X-Echo".into(), PH.into());
                    q
                }),
                ("second Authorization spelling", Some(read_call()), {
                    let mut q = request("GET", MSGS, PH, b"");
                    q.headers
                        .insert("authorization".into(), "Bearer evil".into());
                    q
                }),
            ];
            for (name, call, req) in cases {
                let r = rig(call, &[], false);
                let err = r.gate.prepare(req).await.expect_err(name);
                let GateError::Denied(text) = err else {
                    panic!("{name}: not a denial: {err:?}");
                };
                assert!(text.contains("declared shape"), "{name}: {text}");
                assert!(
                    !text.contains(PH) && !text.contains(TOKEN),
                    "{name}: {text}"
                );
                assert_eq!(
                    r.source.attached.load(Ordering::SeqCst),
                    0,
                    "{name}: attach ran"
                );
                assert_eq!(r.lookups.0.load(Ordering::SeqCst), 0, "{name}: DNS ran");
                let seen = r.seen.lock().unwrap();
                assert_eq!(seen.len(), 1, "{name}: one DENY event: {seen:?}");
                assert_eq!(seen[0].2, "DENY", "{name}");
                assert!(seen[0].3.starts_with("rule="), "{name}: {}", seen[0].3);
            }
        }

        #[tokio::test]
        async fn the_event_names_the_tool() {
            let r = rig(Some(read_call()), &[], false);
            r.gate
                .prepare(request(
                    "POST",
                    "https://graph.microsoft.com/v1.0/me/sendMail",
                    PH,
                    b"",
                ))
                .await
                .unwrap_err();
            let seen = r.seen.lock().unwrap();
            assert!(
                seen[0].3.contains("tool:m365_probe_search egress"),
                "{}",
                seen[0].3
            );
            assert_eq!(seen[0].0, "graph.microsoft.com");
        }

        #[tokio::test]
        async fn a_denied_destination_never_reaches_attach() {
            let r = rig(Some(read_call()), &["graph.microsoft.com"], false);
            let err = r
                .gate
                .prepare(request("GET", MSGS, PH, b""))
                .await
                .unwrap_err();
            assert!(matches!(err, GateError::Denied(_)), "{err:?}");
            assert_eq!(r.source.attached.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn a_window_that_closes_between_authorize_and_attach_attaches_nothing() {
            let r = rig(Some(read_call()), &[], true);
            let err = r
                .gate
                .prepare(request("GET", MSGS, PH, b""))
                .await
                .unwrap_err();
            let GateError::Denied(text) = err else {
                panic!()
            };
            assert!(text.contains("declared shape"), "{text}");
            assert_eq!(r.source.attached.load(Ordering::SeqCst), 1);
            let seen = r.seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].2, "DENY");
        }

        #[tokio::test]
        async fn requests_without_a_connection_placeholder_are_untouched() {
            let r = rig(None, &[], false);
            let out = r
                .gate
                .prepare(request(
                    "GET",
                    "https://example.com/x",
                    "Bearer user-supplied",
                    b"",
                ))
                .await
                .unwrap();
            assert_eq!(out.headers["Authorization"], "Bearer user-supplied");
            assert_eq!(r.source.attached.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn a_static_binding_with_the_same_placeholder_never_wins() {
            let mut r = rig(Some(read_call()), &[], false);
            let static_secrets = Arc::new(crate::secrets::InMemorySecretProvider::new());
            static_secrets.insert("static/ref", "static-secret-value");
            let binding = SecretBinding {
                binding_id: "m365_graph".into(),
                placeholder: PH.into(),
                secret_ref: "static/ref".into(),
                allowed_destinations: vec!["*".into()],
                inject_at: Some(vam_spec::v1::InjectionSite {
                    r#type: vam_spec::v1::InjectionSiteType::InjectionSiteBearerToken as i32,
                    key: "Authorization".into(),
                }),
            };
            r.gate.secret_provider = static_secrets;
            r.gate.bindings = Arc::new(Mutex::new(vec![binding]));
            let out = r.gate.prepare(request("GET", MSGS, PH, b"")).await.unwrap();
            assert_eq!(out.headers["Authorization"], format!("Bearer {TOKEN}"));
            // And outside any window it is refused, not swapped from the static row.
            r.source.envelope.window().clear();
            let err = r
                .gate
                .prepare(request("GET", MSGS, PH, b""))
                .await
                .unwrap_err();
            assert!(matches!(err, GateError::Denied(_)), "{err:?}");
        }

        #[tokio::test]
        async fn the_token_is_scrubbed_from_a_response() {
            let r = rig(Some(read_call()), &[], false);
            let resp = EgressResponse {
                status_code: 200,
                headers: HashMap::from([("x-echo".to_string(), format!("Bearer {TOKEN}"))]),
                body: format!("{{\"auth\":\"{TOKEN}\"}}").into_bytes(),
                error: None,
            };
            let out = r.gate.finish(resp).await.unwrap();
            let body = String::from_utf8(out.body).unwrap();
            assert!(!body.contains(TOKEN) && body.contains(PH), "{body}");
            assert!(!out.headers["x-echo"].contains(TOKEN));
        }

        #[tokio::test]
        async fn a_url_that_will_not_parse_is_refused_before_the_envelope_matters() {
            let r = rig(Some(read_call()), &[], false);
            let err = r
                .gate
                .prepare(request("GET", "https://", PH, b""))
                .await
                .unwrap_err();
            assert!(matches!(err, GateError::Denied(_)), "{err:?}");
            assert_eq!(r.source.attached.load(Ordering::SeqCst), 0);
        }

        /// A record of every connection and request head an upstream saw.
        #[cfg(feature = "process-guest")]
        async fn spawn_recording_http() -> (SocketAddr, Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let heads = Arc::new(Mutex::new(Vec::new()));
            let conns = Arc::new(AtomicUsize::new(0));
            let (h, c) = (Arc::clone(&heads), Arc::clone(&conns));
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        break;
                    };
                    c.fetch_add(1, Ordering::SeqCst);
                    let h = Arc::clone(&h);
                    tokio::spawn(async move {
                        let mut collected = Vec::new();
                        let mut buf = [0u8; 1024];
                        while !collected.windows(4).any(|w| w == b"\r\n\r\n") {
                            let n = sock.read(&mut buf).await.unwrap_or(0);
                            if n == 0 {
                                return;
                            }
                            collected.extend_from_slice(&buf[..n]);
                        }
                        h.lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&collected).into_owned());
                        let body = format!("{{\"echo\":\"Bearer {TOKEN}\"}}");
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(body.as_bytes()).await;
                    });
                }
            });
            (addr, heads, conns)
        }

        #[cfg(feature = "process-guest")]
        #[tokio::test]
        async fn redirected_host_over_port_54_sends_the_token_and_refusals_dial_nothing() {
            let (addr, heads, conns) = spawn_recording_http().await;
            let mut r = rig(Some(read_call()), &[], false);
            r.gate = r
                .gate
                .with_upstream_redirect("graph.microsoft.com", &format!("http://{addr}"));
            let events = Arc::clone(&r.seen);
            let server = Arc::new(EgressBrokerServer::shared(Arc::new(r.gate)));
            let (_dir, client) = connect_broker(Arc::clone(&server)).await;

            let ok = client
                .dispatch(&request("GET", &format!("{MSGS}?$top=1"), PH, b""))
                .await
                .unwrap();
            assert!(ok.error.is_none(), "{:?}", ok.error);
            assert_eq!(ok.status_code, 200);
            let body = String::from_utf8(ok.body).unwrap();
            assert!(
                !body.contains(TOKEN),
                "token scrubbed from the reply: {body}"
            );
            let heads_now = heads.lock().unwrap().clone();
            assert_eq!(heads_now.len(), 1);
            assert!(
                heads_now[0].starts_with("GET /v1.0/me/messages?$top=1 "),
                "{}",
                heads_now[0]
            );
            let lower = heads_now[0].to_ascii_lowercase();
            assert!(lower.contains(&format!("authorization: bearer {TOKEN}").to_ascii_lowercase()));
            assert!(!heads_now[0].contains(PH));

            let refused = client
                .dispatch(&request(
                    "POST",
                    "https://graph.microsoft.com/v1.0/me/sendMail",
                    PH,
                    b"{}",
                ))
                .await
                .unwrap();
            let err = refused.error.expect("refused over port 54");
            assert!(err.message.contains("declared shape"), "{}", err.message);
            assert_eq!(conns.load(Ordering::SeqCst), 1, "a refusal dials nothing");
            let events = events.lock().unwrap();
            let deny: Vec<_> = events.iter().filter(|e| e.2 == "DENY").collect();
            assert_eq!(deny.len(), 1, "{events:?}");
            assert!(deny[0].3.contains("tool:m365_probe_search egress"));
            assert_eq!(deny[0].0, "graph.microsoft.com");
        }

        #[cfg(feature = "process-guest")]
        #[tokio::test]
        async fn the_redirect_pins_one_host_and_never_skips_the_authorizer() {
            let (addr, _heads, _conns) = spawn_recording_http().await;
            let r = rig(None, &["graph.microsoft.com"], false);
            let gate = r
                .gate
                .with_upstream_redirect("graph.microsoft.com", &format!("http://{addr}"));
            // The authorizer still decides for the redirected host.
            let err = gate
                .check_destination("graph.microsoft.com", 443, "https")
                .await
                .unwrap_err();
            assert!(matches!(err, GateError::Denied(_)), "{err:?}");
            // Any other host keeps DNS and the IP floor: a name that resolves to loopback is denied.
            let r2 = rig(None, &[], false);
            let gate2 = r2
                .gate
                .with_upstream_redirect("graph.microsoft.com", &format!("http://{addr}"))
                .with_resolver(resolver(
                    "other.example",
                    vec!["127.0.0.1".parse().unwrap()],
                ));
            let err = gate2
                .check_destination("other.example", 443, "https")
                .await
                .unwrap_err();
            assert!(matches!(err, GateError::Denied(_)), "{err:?}");
            // The redirected host resolves to the loopback server without DNS.
            let allowed = gate2
                .check_destination("graph.microsoft.com", 443, "https")
                .await
                .unwrap();
            assert_eq!(allowed.addr, addr);
        }

        #[cfg(feature = "process-guest")]
        #[test]
        fn the_redirect_refuses_anything_but_a_loopback_http_url() {
            for bad in [
                "https://127.0.0.1:9",
                "http://example.com:9",
                "http://10.0.0.5:9",
                "http://127.0.0.1",
                "http://u:p@127.0.0.1:9",
                "not a url",
            ] {
                let r = rig(None, &[], false);
                let gate = r.gate.with_upstream_redirect("graph.microsoft.com", bad);
                assert!(gate.redirect_for("graph.microsoft.com").is_none(), "{bad}");
            }
            let r = rig(None, &[], false);
            let gate = r
                .gate
                .with_upstream_redirect("Graph.Microsoft.com.", "http://127.0.0.1:9");
            assert!(gate.redirect_for("graph.microsoft.com").is_some());
            assert!(gate.redirect_for("evil.microsoft.com").is_none());
        }
    }
}
