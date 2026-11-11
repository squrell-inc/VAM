//! In-memory token-bucket rate limits for the HTTP API (IT-2.4).
//!
//! Two layers share one [`RateLimiter`]:
//! - [`key_layer`] sits after auth. Every authenticated request is charged to its key's bucket;
//!   `POST /v1/runs` and `POST /v1/sessions` are also charged to a smaller per-key create bucket.
//! - [`ip_layer`] sits before auth. A per-peer-IP bucket is charged only by requests that carry no
//!   valid key (an auth failure, or a login call), so a whole CI suite on one address with valid
//!   keys never shares it. Once an address is out of tokens, a request with no valid key is
//!   refused before it reaches auth.
//!
//! `/v1/health`, `/openapi.yaml` and `/docs` are never limited. `X-Forwarded-For` is ignored: the
//! peer address is the only identity the daemon can trust. State is per node and resets on restart.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderValue, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::auth::AuthContext;
use crate::workflow::SqliteWorkflowStore;

/// Reason header on every 429.
pub const REASON_HEADER: &str = "X-VAM-Rate-Limited-Reason";
pub const REASON_KEY: &str = "key-rate";
pub const REASON_KEY_CREATE: &str = "key-create-rate";
pub const REASON_IP_AUTH: &str = "ip-auth-rate";

/// Most per-IP buckets kept. Past it, untracked addresses share one overflow bucket.
pub const MAX_IP_BUCKETS: usize = 50_000;
/// Per-key maps are swept of idle (full) buckets above this size.
const KEY_SWEEP_AT: usize = 10_000;

/// What the node enforces. Integers so it can sit in `IsolationInputs` (`Copy`, `Eq`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub key_rps: u32,
    pub key_burst: u32,
    pub create_rps: u32,
    pub create_burst: u32,
    pub ip_rps: u32,
    pub ip_burst: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            key_rps: 30,
            key_burst: 100,
            create_rps: 2,
            create_burst: 20,
            ip_rps: 5,
            ip_burst: 20,
        }
    }
}

impl RateLimitConfig {
    /// The line the node and the receipt report: `on (30/s burst 100, runs 2/s burst 20)` or `off`.
    pub fn describe(&self) -> String {
        if !self.enabled {
            return "off".to_string();
        }
        format!(
            "on ({}/s burst {}, runs {}/s burst {})",
            self.key_rps, self.key_burst, self.create_rps, self.create_burst
        )
    }
}

static INSTALLED: OnceLock<RateLimitConfig> = OnceLock::new();

/// Records the node's config (once) and logs it. A node with limits off logs a WARN.
pub fn install(cfg: RateLimitConfig) {
    if INSTALLED.set(cfg).is_ok() {
        if cfg.enabled {
            tracing::info!(limits = cfg.describe().as_str(), "API rate limits");
        } else {
            tracing::warn!(
                "API rate limits are OFF (VAMD_RATE_LIMIT=off): a client can flood this node"
            );
        }
    }
}

/// The installed config; the defaults (on) when nothing installed it.
pub fn current() -> RateLimitConfig {
    INSTALLED.get().copied().unwrap_or_default()
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn full(burst: f64, now: Instant) -> Self {
        Self {
            tokens: burst,
            last: now,
        }
    }

    fn refill(&mut self, now: Instant, rps: f64, burst: f64) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * rps).min(burst);
        self.last = now;
    }

    /// Takes one token, or says how long until there is one.
    fn take(&mut self, now: Instant, rps: f64, burst: f64) -> Result<(), Duration> {
        self.refill(now, rps, burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            Err(self.wait(rps))
        }
    }

    fn wait(&self, rps: f64) -> Duration {
        Duration::from_secs_f64(((1.0 - self.tokens) / rps).max(0.0))
    }

    fn is_full(&self, now: Instant, rps: f64, burst: f64) -> bool {
        self.tokens + now.saturating_duration_since(self.last).as_secs_f64() * rps >= burst
    }
}

/// A refusal: which bucket, and how long to wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: &'static str,
    pub retry_after_secs: u64,
}

fn secs(wait: Duration) -> u64 {
    (wait.as_secs_f64().ceil() as u64).max(1)
}

/// Peer identity for the IP bucket. IPv6 is masked to its /64.
fn ip_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(o.into())
        }
    }
}

struct IpState {
    map: HashMap<IpAddr, Bucket>,
    overflow: Bucket,
}

pub struct RateLimiter {
    cfg: RateLimitConfig,
    store: Option<Arc<SqliteWorkflowStore>>,
    keys: Mutex<HashMap<String, Bucket>>,
    creates: Mutex<HashMap<String, Bucket>>,
    ips: Mutex<IpState>,
    ip_cap: usize,
}

impl RateLimiter {
    pub fn new(cfg: RateLimitConfig, store: Option<Arc<SqliteWorkflowStore>>) -> Self {
        Self::with_ip_cap(cfg, store, MAX_IP_BUCKETS)
    }

    pub fn with_ip_cap(
        cfg: RateLimitConfig,
        store: Option<Arc<SqliteWorkflowStore>>,
        ip_cap: usize,
    ) -> Self {
        Self {
            cfg,
            store,
            keys: Mutex::new(HashMap::new()),
            creates: Mutex::new(HashMap::new()),
            ips: Mutex::new(IpState {
                map: HashMap::new(),
                overflow: Bucket::full(cfg.ip_burst as f64, Instant::now()),
            }),
            ip_cap,
        }
    }

    /// The limiter `router()` uses: the installed config (the defaults when none).
    pub fn installed(store: Arc<SqliteWorkflowStore>) -> Arc<Self> {
        Arc::new(Self::new(current(), Some(store)))
    }

    pub fn config(&self) -> RateLimitConfig {
        self.cfg
    }

    /// Charges an authenticated request to `key_id`; `create` also charges the create bucket.
    pub fn check_key(&self, key_id: &str, create: bool, now: Instant) -> Result<(), Refusal> {
        if !self.cfg.enabled {
            return Ok(());
        }
        let c = &self.cfg;
        take_keyed(
            &self.keys,
            key_id,
            now,
            c.key_rps as f64,
            c.key_burst as f64,
        )
        .map_err(|w| Refusal {
            reason: REASON_KEY,
            retry_after_secs: secs(w),
        })?;
        if create {
            take_keyed(
                &self.creates,
                key_id,
                now,
                c.create_rps as f64,
                c.create_burst as f64,
            )
            .map_err(|w| Refusal {
                reason: REASON_KEY_CREATE,
                retry_after_secs: secs(w),
            })?;
        }
        Ok(())
    }

    /// Whether `ip` is out of tokens (does not charge).
    pub fn ip_exhausted(&self, ip: IpAddr, now: Instant) -> Option<Refusal> {
        if !self.cfg.enabled {
            return None;
        }
        let (rps, burst) = (self.cfg.ip_rps as f64, self.cfg.ip_burst as f64);
        let mut s = self.ips.lock().unwrap_or_else(|e| e.into_inner());
        let key = ip_key(ip);
        let tracked = s.map.contains_key(&key);
        if !tracked && s.map.len() < self.ip_cap {
            return None;
        }
        let b = if tracked {
            s.map.get_mut(&key)?
        } else {
            // An address the map does not hold is as limited as the overflow bucket says.
            &mut s.overflow
        };
        b.refill(now, rps, burst);
        (b.tokens < 1.0).then(|| Refusal {
            reason: REASON_IP_AUTH,
            retry_after_secs: secs(b.wait(rps)),
        })
    }

    /// Charges `ip` one token for a request that had no valid key.
    pub fn charge_ip(&self, ip: IpAddr, now: Instant) {
        if !self.cfg.enabled {
            return;
        }
        let (rps, burst) = (self.cfg.ip_rps as f64, self.cfg.ip_burst as f64);
        let key = ip_key(ip);
        let mut s = self.ips.lock().unwrap_or_else(|e| e.into_inner());
        if !s.map.contains_key(&key) && s.map.len() >= self.ip_cap {
            s.map.retain(|_, b| !b.is_full(now, rps, burst));
        }
        if s.map.contains_key(&key) || s.map.len() < self.ip_cap {
            let b = s.map.entry(key).or_insert_with(|| Bucket::full(burst, now));
            let _ = b.take(now, rps, burst);
        } else {
            let _ = s.overflow.take(now, rps, burst);
        }
    }

    fn valid_bearer(&self, req: &Request<Body>) -> bool {
        let Some(store) = &self.store else {
            return false;
        };
        let token = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|s| !s.is_empty());
        token.is_some_and(|t| store.authenticate_bearer(t).is_ok())
    }
}

fn take_keyed(
    map: &Mutex<HashMap<String, Bucket>>,
    key: &str,
    now: Instant,
    rps: f64,
    burst: f64,
) -> Result<(), Duration> {
    let mut m = map.lock().unwrap_or_else(|e| e.into_inner());
    if m.len() >= KEY_SWEEP_AT && !m.contains_key(key) {
        m.retain(|_, b| !b.is_full(now, rps, burst));
    }
    m.entry(key.to_string())
        .or_insert_with(|| Bucket::full(burst, now))
        .take(now, rps, burst)
}

fn exempt(path: &str) -> bool {
    path == "/v1/health" || path == "/openapi.yaml" || path == "/docs" || path.starts_with("/docs/")
}

fn is_login(path: &str) -> bool {
    path == "/v1/login" || path == "/v1/login/refresh"
}

fn is_create(method: &Method, path: &str) -> bool {
    method == Method::POST && (path == "/v1/runs" || path == "/v1/sessions")
}

/// Marks a response that went through the key layer, so a handler's 403 is not an auth failure.
#[derive(Clone, Copy)]
struct Authenticated;

fn refuse(r: &Refusal) -> Response {
    let body = serde_json::json!({
        "code": "RESOURCE_EXHAUSTED",
        "message": format!("rate limit exceeded ({}); retry after {}s", r.reason, r.retry_after_secs),
        "retryable": true,
    });
    let mut resp = (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
    let h = resp.headers_mut();
    h.insert(
        axum::http::header::RETRY_AFTER,
        HeaderValue::from(r.retry_after_secs),
    );
    h.insert(REASON_HEADER, HeaderValue::from_static(r.reason));
    resp
}

/// Inner layer, after auth: the per-key buckets.
pub async fn key_layer(
    State(limiter): State<Arc<RateLimiter>>,
    req: Request<Body>,
    next: Next<Body>,
) -> Response {
    let Some(auth) = req.extensions().get::<AuthContext>() else {
        return next.run(req).await;
    };
    let create = is_create(req.method(), req.uri().path());
    let key_id = auth.key.record.key_id.clone();
    let mut resp = match limiter.check_key(&key_id, create, Instant::now()) {
        Ok(()) => next.run(req).await,
        Err(r) => refuse(&r),
    };
    resp.extensions_mut().insert(Authenticated);
    resp
}

/// Outer layer, before auth: the per-IP bucket for requests with no valid key.
pub async fn ip_layer(
    State(limiter): State<Arc<RateLimiter>>,
    req: Request<Body>,
    next: Next<Body>,
) -> Response {
    let path = req.uri().path().to_string();
    if !limiter.cfg.enabled || exempt(&path) {
        return next.run(req).await;
    }
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .unwrap_or(IpAddr::from([0, 0, 0, 0]));
    if let Some(r) = limiter.ip_exhausted(ip, Instant::now()) {
        // A valid key is never held back by its address's failures.
        if !limiter.valid_bearer(&req) {
            return refuse(&r);
        }
    }
    let resp = next.run(req).await;
    let authenticated = resp.extensions().get::<Authenticated>().is_some();
    let rejected = matches!(
        resp.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    );
    if is_login(&path) || (!authenticated && rejected) {
        limiter.charge_ip(ip, Instant::now());
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RateLimitConfig {
        RateLimitConfig {
            enabled: true,
            key_rps: 10,
            key_burst: 5,
            create_rps: 1,
            create_burst: 2,
            ip_rps: 2,
            ip_burst: 3,
        }
    }

    fn limiter() -> RateLimiter {
        RateLimiter::new(cfg(), None)
    }

    #[test]
    fn burst_then_refusal_then_refill() {
        let l = limiter();
        let t0 = Instant::now();
        for _ in 0..5 {
            l.check_key("k", false, t0).unwrap();
        }
        let r = l.check_key("k", false, t0).unwrap_err();
        assert_eq!(r.reason, REASON_KEY);
        assert_eq!(r.retry_after_secs, 1);
        // 10/s: 100 ms later one token is back.
        l.check_key("k", false, t0 + Duration::from_millis(110))
            .unwrap();
        assert!(l
            .check_key("k", false, t0 + Duration::from_millis(110))
            .is_err());
        // The bucket never refills past the burst.
        let later = t0 + Duration::from_secs(60);
        for _ in 0..5 {
            l.check_key("k", false, later).unwrap();
        }
        assert!(l.check_key("k", false, later).is_err());
    }

    #[test]
    fn keys_do_not_share_a_bucket() {
        let l = limiter();
        let t0 = Instant::now();
        for _ in 0..5 {
            l.check_key("flooder", false, t0).unwrap();
        }
        assert!(l.check_key("flooder", false, t0).is_err());
        l.check_key("admin", false, t0).unwrap();
    }

    #[test]
    fn create_has_its_own_smaller_bucket() {
        let l = limiter();
        let t0 = Instant::now();
        l.check_key("k", true, t0).unwrap();
        l.check_key("k", true, t0).unwrap();
        let r = l.check_key("k", true, t0).unwrap_err();
        assert_eq!(r.reason, REASON_KEY_CREATE);
        // Plain requests still pass: only the create bucket is empty.
        l.check_key("k", false, t0).unwrap();
        // 1/s: a second later a creation passes again.
        l.check_key("k", true, t0 + Duration::from_millis(1100))
            .unwrap();
    }

    #[test]
    fn retry_after_rounds_up_to_whole_seconds() {
        let l = RateLimiter::new(
            RateLimitConfig {
                key_rps: 1,
                key_burst: 1,
                ..cfg()
            },
            None,
        );
        let t0 = Instant::now();
        l.check_key("k", false, t0).unwrap();
        let r = l.check_key("k", false, t0).unwrap_err();
        assert_eq!(r.retry_after_secs, 1);
        let l = RateLimiter::new(
            RateLimitConfig {
                key_rps: 1,
                key_burst: 1,
                ..cfg()
            },
            None,
        );
        l.check_key("k", false, t0).unwrap();
        // Half a token short at 1/s is still one whole second to wait, never zero.
        let r = l
            .check_key("k", false, t0 + Duration::from_millis(500))
            .unwrap_err();
        assert_eq!(r.retry_after_secs, 1);
    }

    #[test]
    fn ip_bucket_is_charged_only_when_asked_and_blocks_when_empty() {
        let l = limiter();
        let t0 = Instant::now();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        // Valid-key traffic never calls charge_ip: the bucket stays untouched.
        assert!(l.ip_exhausted(ip, t0).is_none());
        for _ in 0..3 {
            assert!(l.ip_exhausted(ip, t0).is_none());
            l.charge_ip(ip, t0);
        }
        let r = l.ip_exhausted(ip, t0).expect("empty after the burst");
        assert_eq!(r.reason, REASON_IP_AUTH);
        // Another address is unaffected, and 2/s refills the first one.
        assert!(l.ip_exhausted("10.0.0.2".parse().unwrap(), t0).is_none());
        assert!(l
            .ip_exhausted(ip, t0 + Duration::from_millis(600))
            .is_none());
    }

    #[test]
    fn ipv6_is_masked_to_its_64() {
        let l = limiter();
        let t0 = Instant::now();
        for _ in 0..3 {
            l.charge_ip("2001:db8::1".parse().unwrap(), t0);
        }
        assert!(l
            .ip_exhausted("2001:db8::ffff".parse().unwrap(), t0)
            .is_some());
        assert!(l
            .ip_exhausted("2001:db8:0:1::1".parse().unwrap(), t0)
            .is_none());
    }

    #[test]
    fn ip_cap_bounds_memory_and_untracked_addresses_share_one_overflow_bucket() {
        let l = RateLimiter::with_ip_cap(cfg(), None, 2);
        let t0 = Instant::now();
        // Two addresses fill the map, each with a charged (not full) bucket.
        l.charge_ip("10.0.0.1".parse().unwrap(), t0);
        l.charge_ip("10.0.0.2".parse().unwrap(), t0);
        // Every further address shares one bucket of the per-IP size (burst 3).
        for i in 3..6u8 {
            let ip: IpAddr = format!("10.0.0.{i}").parse().unwrap();
            assert!(l.ip_exhausted(ip, t0).is_none());
            l.charge_ip(ip, t0);
        }
        let ip7: IpAddr = "10.0.0.7".parse().unwrap();
        assert!(
            l.ip_exhausted(ip7, t0).is_some(),
            "overflow bucket is shared"
        );
        let s = l.ips.lock().unwrap();
        assert_eq!(s.map.len(), 2, "the map did not grow past the cap");
    }

    #[test]
    fn full_ip_buckets_are_swept_to_make_room() {
        let l = RateLimiter::with_ip_cap(cfg(), None, 2);
        let t0 = Instant::now();
        l.charge_ip("10.0.0.1".parse().unwrap(), t0);
        l.charge_ip("10.0.0.2".parse().unwrap(), t0);
        let later = t0 + Duration::from_secs(30);
        // Both refilled to full, so a new address replaces them instead of overflowing.
        l.charge_ip("10.0.0.3".parse().unwrap(), later);
        let s = l.ips.lock().unwrap();
        assert!(s.map.contains_key(&"10.0.0.3".parse::<IpAddr>().unwrap()));
        assert!(s.map.len() <= 2);
    }

    #[test]
    fn off_never_limits_and_reports_off() {
        let off = RateLimitConfig {
            enabled: false,
            ..cfg()
        };
        let l = RateLimiter::new(off, None);
        let t0 = Instant::now();
        for _ in 0..1000 {
            l.check_key("k", true, t0).unwrap();
        }
        l.charge_ip("10.0.0.1".parse().unwrap(), t0);
        assert!(l.ip_exhausted("10.0.0.1".parse().unwrap(), t0).is_none());
        assert_eq!(off.describe(), "off");
        assert_eq!(
            RateLimitConfig::default().describe(),
            "on (30/s burst 100, runs 2/s burst 20)"
        );
    }

    #[test]
    fn exemptions_and_create_paths() {
        assert!(exempt("/v1/health") && exempt("/docs") && exempt("/openapi.yaml"));
        assert!(!exempt("/v1/node") && !exempt("/v1/login"));
        assert!(is_create(&Method::POST, "/v1/runs"));
        assert!(is_create(&Method::POST, "/v1/sessions"));
        assert!(!is_create(&Method::GET, "/v1/runs"));
        assert!(!is_create(&Method::POST, "/v1/runs/x/cancel"));
    }

    #[test]
    fn refusal_carries_retry_after_and_reason() {
        let r = refuse(&Refusal {
            reason: REASON_KEY,
            retry_after_secs: 3,
        });
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(r.headers()["retry-after"], "3");
        assert_eq!(r.headers()[REASON_HEADER], "key-rate");
    }
}
