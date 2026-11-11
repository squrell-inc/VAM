use std::env;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use vam_telemetry::{
    parse_flag_default_false, parse_otlp_enabled, CollectorTlsConfig, OtlpConfig, RetentionPolicy,
    TailSamplingPolicy, TelemetryCapture, TenantId, DEFAULT_OTLP_ENDPOINT, DEFAULT_TENANT_ID,
    ENV_CLOUDEVENTS_MIRROR_OTLP, ENV_OTLP_ENABLED, ENV_OTLP_ENDPOINT, ENV_OTLP_SLOW_MS,
    ENV_OTLP_SUCCESS_SAMPLE_RATE, ENV_TELEMETRY_CAPTURE, ENV_TENANT_ID,
};

use crate::egress::{
    EgressConfig, DEFAULT_SPOOL_DIR, ENV_CLOUDEVENTS_URL, ENV_OPENLINEAGE_URL, ENV_SPOOL_DIR,
};

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub db_path: PathBuf,
    /// `VAMD_FIRECRACKER_BIN`; empty when unset (a VM node refuses to start, see
    /// [`missing_vm_node_vars`]).
    pub firecracker_bin: PathBuf,
    /// `VAMD_JAILER_BIN`; empty when unset (required while the jailer is on).
    pub jailer_bin: PathBuf,
    /// Idle-session TTL in seconds (`VAMD_SESSION_IDLE_TTL_SECS`, default 86400); `None` only
    /// when set to `off`.
    pub session_idle_ttl_secs: Option<u64>,
    /// A warning for `vamd` to log at start: a bad TTL value, or cleanup switched off.
    pub session_idle_ttl_warning: Option<String>,
    /// Run Firecracker under the jailer. On by default; `--no-jailer` or
    /// `VAMD_USE_JAILER=0` is the explicit opt-out (logged, and the node reports
    /// `jailer off`).
    pub use_jailer: bool,
    /// uid and gid the jailed Firecracker drops to (`VAMD_JAILER_UID`, `VAMD_JAILER_GID`).
    /// Default 65400:65400, a dedicated unprivileged id; 0 is refused at start.
    pub jailer_uid: u32,
    pub jailer_gid: u32,
    /// Jailer cgroup version (`VAMD_JAILER_CGROUP_VERSION`, default 2).
    pub jailer_cgroup_version: u8,
    pub listen_addr: SocketAddr,
    /// Localhost-only telemetry gRPC (`StreamEvents`). No PDP on this socket in Phase 0.
    pub telemetry_listen_addr: SocketAddr,
    /// Guest tool-body capture tier (`VAMD_TELEMETRY_CAPTURE`). Default `digest`.
    pub telemetry_capture: TelemetryCapture,
    /// Host OTLP export (loopback collector). Default enabled.
    pub otlp_enabled: bool,
    /// OTLP/HTTP endpoint. Default `http://127.0.0.1:4318` (gRPC 4317 unused: tonic 0.10).
    pub otlp_endpoint: String,
    pub otlp_success_sample_rate: f64,
    pub otlp_slow_ms: u64,
    /// CloudEvents HTTP webhook. Empty = files only under `spool_dir`.
    pub cloudevents_url: String,
    /// JSONL + retry spool root. Default `/var/lib/vam/spool`.
    pub spool_dir: PathBuf,
    /// Optional OTLP-logs mirror of event type/run_id/sequence. Default off.
    pub cloudevents_mirror_otlp: bool,
    /// OpenLineage HTTP endpoint. Empty = `lineage.jsonl` only.
    pub openlineage_url: String,
    /// Single-node tenant (`VAMD_TENANT_ID`, default `local`).
    pub tenant_id: String,
    /// OTLP collector mTLS / expected SPIFFE (from env).
    pub otlp_tls: CollectorTlsConfig,
    pub otlp_disabled_reason: Option<String>,
    pub retention_policy: RetentionPolicy,
    pub tls_cert_path: Option<PathBuf>,
    pub tls_key_path: Option<PathBuf>,
    /// Serve the API over plain HTTP (`--insecure-dev` or `VAMD_INSECURE_DEV`).
    /// Off by default: the API is served over TLS. Dev hosts only.
    pub insecure_dev: bool,
    /// Extra names or IPs for the self-signed certificate (`VAMD_TLS_SANS`, comma-separated).
    pub tls_extra_sans: Vec<String>,
    pub bootstrap_api_key: Option<String>,
    /// When set, `vamd` hashes a new password for this user (from
    /// `VAMD_RESET_PASSWORD`) and exits. Admin-only; no email.
    pub reset_password_user: Option<String>,
    /// Username for headless first-admin bootstrap (`VAMD_BOOTSTRAP_ADMIN`
    /// or `--bootstrap-admin`). Password comes from
    /// `VAMD_BOOTSTRAP_ADMIN_PASSWORD`.
    pub bootstrap_admin: Option<String>,
    /// When true, apply first-admin bootstrap and exit (CLI `--bootstrap-admin`).
    pub bootstrap_admin_oneshot: bool,
    /// `vamd --init`: create the state dir, DB, TLS material and signing keys, mint the first
    /// admin key when none exists, print it to stdout once and exit.
    pub init_oneshot: bool,
    /// `vamd --preflight`: print the node's start rules as JSON and exit.
    pub preflight_oneshot: bool,
    /// `vamd --preflight --root DIR`: a test seam that is read by `--preflight` only.
    pub preflight_root: Option<PathBuf>,
    /// Tool-level HITL approval lifetime in seconds. Default 3600.
    /// `VAMD_TOOL_HITL_TIMEOUT_SECS` or `--tool-hitl-timeout-secs`.
    pub tool_hitl_timeout_secs: i64,
    /// `egress.passthroughHosts`. Empty by default. These hosts stay
    /// `CONNECT` passthrough on port 443. `VAMD_EGRESS_PASSTHROUGH_HOSTS`
    /// or `--egress-passthrough-hosts` (comma-separated).
    pub egress_passthrough_hosts: Vec<String>,
    /// Per-session egress byte cap (`egress.maxBytes`). Counts tunnel and HTTP
    /// bytes in both directions. `0` disables the cap. Default 1 GiB.
    /// `VAMD_EGRESS_MAX_BYTES` or `--egress-max-bytes`.
    pub egress_max_bytes: u64,
    /// Registry domain of this node's org (`registry_domain` on `GET /v1/me`).
    /// Empty means `local.vam.dev`. `VAMD_REGISTRY_DOMAIN` or `--registry-domain`.
    pub registry_domain: String,
    /// Squrell marketplace domain. Empty (the default) means none is set.
    /// `VAMD_MARKETPLACE_DOMAIN` or `--marketplace-domain`.
    pub marketplace_domain: String,
    /// `--connector-client-id <id>=<client id>` values (repeatable). The environment
    /// (`VAMD_CONNECTOR_<ID>_CLIENT_ID`) is read with them by [`Self::connector_config`].
    pub connector_client_id_flags: Vec<String>,
}

pub const ENV_EGRESS_PASSTHROUGH_HOSTS: &str = "VAMD_EGRESS_PASSTHROUGH_HOSTS";

pub const ENV_EGRESS_MAX_BYTES: &str = "VAMD_EGRESS_MAX_BYTES";

pub const ENV_REGISTRY_DOMAIN: &str = "VAMD_REGISTRY_DOMAIN";

pub const ENV_MARKETPLACE_DOMAIN: &str = "VAMD_MARKETPLACE_DOMAIN";

pub const ENV_INSECURE_DEV: &str = "VAMD_INSECURE_DEV";

pub const ENV_TLS_SANS: &str = "VAMD_TLS_SANS";

/// The jailer is on unless `VAMD_USE_JAILER` says `0`, `false`, `off` or `no`. Any other
/// value keeps it on: a typo must not switch the confinement off. (`--no-jailer` is read
/// from the arguments after this, so a flag beats the environment.)
pub fn parse_use_jailer(env_value: Option<&str>) -> bool {
    !env_value
        .map(|v| v.trim().to_ascii_lowercase())
        .is_some_and(|v| matches!(v.as_str(), "0" | "false" | "off" | "no"))
}

/// The environment, then the flags in order (`--use-jailer`, `--no-jailer`): the last
/// one wins, and a flag beats the environment.
pub fn resolve_use_jailer(env_value: Option<&str>, args: &[String]) -> bool {
    let mut on = parse_use_jailer(env_value);
    for a in args.iter().skip(1) {
        match a.as_str() {
            "--use-jailer" => on = true,
            "--no-jailer" => on = false,
            _ => {}
        }
    }
    on
}

/// A `VAMD_USE_JAILER` value that is neither an opt-in nor an opt-out (the jailer stays on).
pub fn unrecognised_use_jailer(env_value: Option<&str>) -> Option<String> {
    let v = env_value?.trim().to_ascii_lowercase();
    let known = ["", "0", "false", "off", "no", "1", "true", "on", "yes"];
    (!known.contains(&v.as_str())).then_some(v)
}

/// A numeric uid or gid from the environment, else the default.
pub fn parse_jailer_id(env_value: Option<&str>, default: u32) -> u32 {
    env_value
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(default)
}

/// True when `--insecure-dev` is among the arguments, or the env value is `1` or `true`.
pub fn parse_insecure_dev(env_value: Option<&str>, args: &[String]) -> bool {
    let env_on = env_value
        .map(|v| v.trim().to_ascii_lowercase())
        .is_some_and(|v| v == "1" || v == "true");
    env_on || args.iter().skip(1).any(|a| a == "--insecure-dev")
}

/// Splits `VAMD_TLS_SANS`: comma-separated, blanks dropped.
pub fn parse_tls_sans(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Default per-session egress byte cap: 1 GiB.
pub const DEFAULT_EGRESS_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// Parse `egress.maxBytes`. Empty or non-numeric input falls back to the
/// default. `0` is a valid value and means unlimited.
pub fn parse_egress_max_bytes(raw: Option<&str>) -> u64 {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return DEFAULT_EGRESS_MAX_BYTES;
    };
    raw.parse::<u64>().unwrap_or(DEFAULT_EGRESS_MAX_BYTES)
}

/// Split `egress.passthroughHosts`. Empty pieces are dropped. Matching is
/// case-insensitive and ignores one trailing dot.
pub fn parse_egress_passthrough_hosts(raw: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for part in raw.split(',') {
        let host = part.trim().trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() || hosts.iter().any(|existing| existing == &host) {
            continue;
        }
        hosts.push(host);
    }
    hosts
}

pub const ENV_TOOL_HITL_TIMEOUT_SECS: &str = "VAMD_TOOL_HITL_TIMEOUT_SECS";

/// Operator tool-HITL lifetime. Empty, non-numeric, non-positive, or over 30
/// days falls back to the one-hour default.
pub fn parse_tool_hitl_timeout_secs(raw: Option<&str>) -> i64 {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return crate::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS;
    };
    match raw.parse::<i64>() {
        Ok(secs) if crate::allowed_models::normalize_tool_hitl_timeout_secs(secs).is_ok() => secs,
        _ => crate::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS,
    }
}

pub const ENV_SESSION_IDLE_TTL_SECS: &str = "VAMD_SESSION_IDLE_TTL_SECS";
pub const DEFAULT_SESSION_IDLE_TTL_SECS: u64 = 86_400;

/// `VAMD_SESSION_IDLE_TTL_SECS`: unset or blank gives 24 h; `off` disables the sweep (returned
/// with a warning); `0`, a negative or a non-numeric value is invalid and falls back to 24 h
/// with a warning naming the value.
pub fn parse_session_idle_ttl(raw: Option<&str>) -> (Option<u64>, Option<String>) {
    let Some(v) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return (Some(DEFAULT_SESSION_IDLE_TTL_SECS), None);
    };
    if v.eq_ignore_ascii_case("off") {
        return (
            None,
            Some(format!(
                "{ENV_SESSION_IDLE_TTL_SECS}=off: idle sessions are never ended; their VMs and workspace locks live until terminated"
            )),
        );
    }
    match v.parse::<u64>() {
        Ok(n) if n > 0 => (Some(n), None),
        _ => (
            Some(DEFAULT_SESSION_IDLE_TTL_SECS),
            Some(format!(
                "{ENV_SESSION_IDLE_TTL_SECS}={v:?} is not a positive number of seconds (use `off` to disable); using {DEFAULT_SESSION_IDLE_TTL_SECS}"
            )),
        ),
    }
}

/// Variables a VM node cannot start without (IT-2.8): no built-in path. The jailer binary is
/// needed only while the jailer is on. `get` reads one variable (the environment in `vamd`);
/// a blank value counts as missing.
pub fn missing_vm_node_vars(
    get: impl Fn(&str) -> Option<String>,
    use_jailer: bool,
) -> Vec<&'static str> {
    let mut required = vec!["VAMD_FIRECRACKER_BIN", "VAM_ROOTFS_PATH"];
    if use_jailer {
        required.push("VAMD_JAILER_BIN");
    }
    required
        .into_iter()
        .filter(|name| get(name).is_none_or(|v| v.trim().is_empty()))
        .collect()
}

/// The one-line start refusal naming every missing variable.
pub fn vm_node_config_error(missing: &[&str]) -> String {
    format!(
        "a VM node needs {} set (vamd has no built-in paths)",
        missing.join(", ")
    )
}

pub const ENV_RATE_LIMIT: &str = "VAMD_RATE_LIMIT";
pub const ENV_RATE_LIMIT_RPS: &str = "VAMD_RATE_LIMIT_RPS";
pub const ENV_RATE_LIMIT_BURST: &str = "VAMD_RATE_LIMIT_BURST";
pub const ENV_RATE_LIMIT_CREATE_RPS: &str = "VAMD_RATE_LIMIT_CREATE_RPS";
pub const ENV_RATE_LIMIT_CREATE_BURST: &str = "VAMD_RATE_LIMIT_CREATE_BURST";
pub const ENV_RATE_LIMIT_IP_RPS: &str = "VAMD_RATE_LIMIT_IP_RPS";
pub const ENV_RATE_LIMIT_IP_BURST: &str = "VAMD_RATE_LIMIT_IP_BURST";

/// API rate limits (IT-2.4). On unless `VAMD_RATE_LIMIT` is `off`, `0`, `false` or `no`; a limit
/// that is empty, non-numeric or zero keeps its default. `get` reads one variable (the
/// environment in `vamd`).
pub fn parse_rate_limits(
    get: impl Fn(&str) -> Option<String>,
) -> crate::api::rate_limit::RateLimitConfig {
    let mut cfg = crate::api::rate_limit::RateLimitConfig::default();
    let num = |name: &str, default: u32| {
        get(name)
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(default)
    };
    cfg.enabled = !matches!(
        get(ENV_RATE_LIMIT)
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
        Some("off" | "0" | "false" | "no")
    );
    cfg.key_rps = num(ENV_RATE_LIMIT_RPS, cfg.key_rps);
    cfg.key_burst = num(ENV_RATE_LIMIT_BURST, cfg.key_burst);
    cfg.create_rps = num(ENV_RATE_LIMIT_CREATE_RPS, cfg.create_rps);
    cfg.create_burst = num(ENV_RATE_LIMIT_CREATE_BURST, cfg.create_burst);
    cfg.ip_rps = num(ENV_RATE_LIMIT_IP_RPS, cfg.ip_rps);
    cfg.ip_burst = num(ENV_RATE_LIMIT_IP_BURST, cfg.ip_burst);
    cfg
}

/// Validate the configured registry and marketplace domains. Blank registry
/// means `local.vam.dev`; blank marketplace means Squrell's `hub.squrell.cloud`
/// (`VAMD_MARKETPLACE_DOMAIN` overrides it, e.g. on a staging node).
pub fn build_registry_domains(
    registry: &str,
    marketplace: &str,
) -> Result<vam_spec::RegistryDomains, vam_spec::RegistryDomainError> {
    let marketplace = match marketplace.trim() {
        "" => vam_spec::DEFAULT_MARKETPLACE_DOMAIN,
        set => set,
    };
    vam_spec::RegistryDomains::new(registry, Some(marketplace))
}

pub fn default_telemetry_listen() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 9090))
}

/// Why the one-shot flags were refused, or `None` when the command line is allowed.
/// `--root` belongs to `--preflight` alone: on any other path it would suggest the daemon
/// can be pointed at another tree, and it cannot.
pub fn one_shot_flag_error(args: &[String]) -> Option<String> {
    let has = |f: &str| args.iter().skip(1).any(|a| a == f);
    if has("--root") && !has("--preflight") {
        return Some("--root is only valid with --preflight".to_string());
    }
    if has("--preflight") && has("--init") {
        return Some("--preflight and --init cannot be combined".to_string());
    }
    None
}

impl DaemonConfig {
    /// The config as it is logged: the bootstrap key's value is never shown.
    pub fn redacted(&self) -> Self {
        let mut shown = self.clone();
        shown.bootstrap_api_key = shown.bootstrap_api_key.map(|_| "<redacted>".to_string());
        shown
    }

    pub fn parse() -> Self {
        // Default values
        let mut db_path = PathBuf::from("/var/lib/vam/state.db");
        let mut firecracker_bin = PathBuf::new();
        let mut jailer_bin = PathBuf::new();
        let (session_idle_ttl_secs, session_idle_ttl_warning) =
            parse_session_idle_ttl(env::var(ENV_SESSION_IDLE_TTL_SECS).ok().as_deref());
        let jailer_env = env::var("VAMD_USE_JAILER").ok();
        let jailer_uid = parse_jailer_id(
            env::var("VAMD_JAILER_UID").ok().as_deref(),
            vam_core::jail::DEFAULT_JAIL_UID,
        );
        let jailer_gid = parse_jailer_id(
            env::var("VAMD_JAILER_GID").ok().as_deref(),
            vam_core::jail::DEFAULT_JAIL_GID,
        );
        let jailer_cgroup_version = env::var("VAMD_JAILER_CGROUP_VERSION")
            .ok()
            .and_then(|v| v.trim().parse::<u8>().ok())
            .unwrap_or(vam_core::jail::DEFAULT_CGROUP_VERSION);
        let mut listen_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 8080));
        let mut telemetry_listen_addr = default_telemetry_listen();
        let mut telemetry_capture = TelemetryCapture::default();
        let mut otlp_enabled = true;
        let mut otlp_endpoint = DEFAULT_OTLP_ENDPOINT.to_string();
        let mut otlp_success_sample_rate = TailSamplingPolicy::default().success_sample_rate;
        let mut otlp_slow_ms = TailSamplingPolicy::default().slow_trace_threshold_ms;
        let mut cloudevents_url = String::new();
        let mut spool_dir = PathBuf::from(DEFAULT_SPOOL_DIR);
        let mut cloudevents_mirror_otlp = false;
        let mut openlineage_url = String::new();
        let mut tenant_id = DEFAULT_TENANT_ID.to_string();
        let mut tls_cert_path = None;
        let mut tls_key_path = None;
        let tls_extra_sans = env::var(ENV_TLS_SANS)
            .map(|v| parse_tls_sans(&v))
            .unwrap_or_default();
        let mut bootstrap_api_key = None;
        let mut reset_password_user = None;
        let mut bootstrap_admin = None;
        let mut bootstrap_admin_oneshot = false;
        let mut init_oneshot = false;
        let mut preflight_oneshot = false;
        let mut preflight_root = None;
        let mut tool_hitl_timeout_secs =
            parse_tool_hitl_timeout_secs(env::var(ENV_TOOL_HITL_TIMEOUT_SECS).ok().as_deref());
        let mut egress_passthrough_hosts = env::var(ENV_EGRESS_PASSTHROUGH_HOSTS)
            .ok()
            .map(|value| parse_egress_passthrough_hosts(&value))
            .unwrap_or_default();
        let mut egress_max_bytes =
            parse_egress_max_bytes(env::var(ENV_EGRESS_MAX_BYTES).ok().as_deref());

        let mut registry_domain = env::var(ENV_REGISTRY_DOMAIN).unwrap_or_default();
        let mut marketplace_domain = env::var(ENV_MARKETPLACE_DOMAIN).unwrap_or_default();
        let mut connector_client_id_flags: Vec<String> = Vec::new();

        // 1. Env variables override
        if let Ok(val) = env::var("VAMD_DB_PATH") {
            db_path = PathBuf::from(val);
        }
        if let Ok(val) = env::var("VAMD_FIRECRACKER_BIN") {
            firecracker_bin = PathBuf::from(val);
        }
        if let Ok(val) = env::var("VAMD_JAILER_BIN") {
            jailer_bin = PathBuf::from(val);
        }
        if let Ok(val) = env::var("VAMD_LISTEN") {
            if let Ok(addr) = val.parse::<SocketAddr>() {
                listen_addr = addr;
            }
        }
        if let Ok(val) = env::var("VAMD_TELEMETRY_LISTEN") {
            if let Ok(addr) = val.parse::<SocketAddr>() {
                telemetry_listen_addr = addr;
            }
        }
        if let Ok(val) = env::var(ENV_TELEMETRY_CAPTURE) {
            telemetry_capture = TelemetryCapture::parse_env_value(&val);
        }
        if let Ok(val) = env::var(ENV_OTLP_ENABLED) {
            otlp_enabled = parse_otlp_enabled(Some(&val));
        }
        if let Ok(val) = env::var(ENV_OTLP_ENDPOINT) {
            if !val.is_empty() {
                otlp_endpoint = val;
            }
        }
        if let Ok(val) = env::var(ENV_OTLP_SUCCESS_SAMPLE_RATE) {
            if let Ok(rate) = val.parse::<f64>() {
                if rate >= 0.0 {
                    otlp_success_sample_rate = rate;
                }
            }
        }
        if let Ok(val) = env::var(ENV_OTLP_SLOW_MS) {
            if let Ok(ms) = val.parse::<u64>() {
                otlp_slow_ms = ms;
            }
        }
        if let Ok(val) = env::var("VAMD_TLS_CERT") {
            tls_cert_path = Some(PathBuf::from(val));
        }
        if let Ok(val) = env::var("VAMD_TLS_KEY") {
            tls_key_path = Some(PathBuf::from(val));
        }
        if let Ok(val) = env::var("VAMD_BOOTSTRAP_API_KEY") {
            if !val.is_empty() {
                bootstrap_api_key = Some(val);
            }
        }
        if let Ok(val) = env::var(crate::password::RESET_PASSWORD_USER_ENV) {
            if !val.is_empty() {
                reset_password_user = Some(val);
            }
        }
        if let Ok(val) = env::var(crate::password::BOOTSTRAP_ADMIN_ENV) {
            if !val.is_empty() {
                bootstrap_admin = Some(val);
            }
        }
        if let Ok(val) = env::var(ENV_CLOUDEVENTS_URL) {
            cloudevents_url = val;
        }
        if let Ok(val) = env::var(ENV_SPOOL_DIR) {
            if !val.is_empty() {
                spool_dir = PathBuf::from(val);
            }
        }
        if let Ok(val) = env::var(ENV_CLOUDEVENTS_MIRROR_OTLP) {
            cloudevents_mirror_otlp = parse_flag_default_false(Some(&val));
        }
        if let Ok(val) = env::var(ENV_OPENLINEAGE_URL) {
            openlineage_url = val;
        }
        if let Ok(val) = env::var(ENV_TENANT_ID) {
            if !val.is_empty() {
                tenant_id = val;
            }
        }

        // 2. CLI flags override (simple precision parser)
        let args: Vec<String> = env::args().collect();
        let use_jailer = resolve_use_jailer(jailer_env.as_deref(), &args);
        if let Some(v) = unrecognised_use_jailer(jailer_env.as_deref()) {
            eprintln!("vamd: VAMD_USE_JAILER={v:?} is not understood; the jailer stays on (use 0 or --no-jailer to opt out)");
        }
        let insecure_dev = parse_insecure_dev(env::var(ENV_INSECURE_DEV).ok().as_deref(), &args);
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--db-path" if i + 1 < args.len() => {
                    db_path = PathBuf::from(&args[i + 1]);
                    i += 2;
                }
                "--firecracker-bin" if i + 1 < args.len() => {
                    firecracker_bin = PathBuf::from(&args[i + 1]);
                    i += 2;
                }
                "--jailer-bin" if i + 1 < args.len() => {
                    jailer_bin = PathBuf::from(&args[i + 1]);
                    i += 2;
                }
                "--listen" if i + 1 < args.len() => {
                    if let Ok(addr) = args[i + 1].parse::<SocketAddr>() {
                        listen_addr = addr;
                    }
                    i += 2;
                }
                // Read by `resolve_use_jailer` below, with the environment.
                "--use-jailer" | "--no-jailer" => {
                    i += 1;
                }
                "--reset-password" if i + 1 < args.len() => {
                    reset_password_user = Some(args[i + 1].clone());
                    i += 2;
                }
                "--bootstrap-admin" if i + 1 < args.len() && !args[i + 1].starts_with('-') => {
                    bootstrap_admin = Some(args[i + 1].clone());
                    bootstrap_admin_oneshot = true;
                    i += 2;
                }
                "--bootstrap-admin" => {
                    bootstrap_admin_oneshot = true;
                    i += 1;
                }
                "--init" => {
                    init_oneshot = true;
                    i += 1;
                }
                "--preflight" => {
                    preflight_oneshot = true;
                    i += 1;
                }
                "--root" if i + 1 < args.len() => {
                    preflight_root = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                }
                "--tool-hitl-timeout-secs" if i + 1 < args.len() => {
                    tool_hitl_timeout_secs = parse_tool_hitl_timeout_secs(Some(&args[i + 1]));
                    i += 2;
                }
                "--egress-passthrough-hosts" if i + 1 < args.len() => {
                    egress_passthrough_hosts = parse_egress_passthrough_hosts(&args[i + 1]);
                    i += 2;
                }
                "--egress-max-bytes" if i + 1 < args.len() => {
                    egress_max_bytes = parse_egress_max_bytes(Some(&args[i + 1]));
                    i += 2;
                }
                "--registry-domain" if i + 1 < args.len() => {
                    registry_domain = args[i + 1].clone();
                    i += 2;
                }
                "--marketplace-domain" if i + 1 < args.len() => {
                    marketplace_domain = args[i + 1].clone();
                    i += 2;
                }
                "--connector-client-id" if i + 1 < args.len() => {
                    connector_client_id_flags.push(args[i + 1].clone());
                    i += 2;
                }
                _ => {
                    i += 1;
                }
            }
        }

        let otlp_tls = CollectorTlsConfig::from_env();
        let resolved = OtlpConfig {
            enabled: otlp_enabled,
            endpoint: otlp_endpoint,
            policy: TailSamplingPolicy {
                success_sample_rate: otlp_success_sample_rate,
                slow_trace_threshold_ms: otlp_slow_ms,
            },
            tls: otlp_tls,
            disabled_reason: None,
        }
        .resolve();

        Self {
            db_path,
            session_idle_ttl_secs,
            session_idle_ttl_warning,
            firecracker_bin,
            jailer_bin,
            use_jailer,
            jailer_uid,
            jailer_gid,
            jailer_cgroup_version,
            listen_addr,
            telemetry_listen_addr,
            telemetry_capture,
            otlp_enabled: resolved.enabled,
            otlp_endpoint: resolved.endpoint.clone(),
            otlp_success_sample_rate: resolved.policy.success_sample_rate,
            otlp_slow_ms: resolved.policy.slow_trace_threshold_ms,
            cloudevents_url,
            spool_dir,
            cloudevents_mirror_otlp,
            openlineage_url,
            tenant_id,
            otlp_tls: resolved.tls.clone(),
            otlp_disabled_reason: resolved.disabled_reason.clone(),
            retention_policy: RetentionPolicy::from_env(),
            tls_cert_path,
            tls_key_path,
            insecure_dev,
            tls_extra_sans,
            bootstrap_api_key,
            reset_password_user,
            bootstrap_admin,
            bootstrap_admin_oneshot,
            init_oneshot,
            preflight_oneshot,
            preflight_root,
            tool_hitl_timeout_secs,
            egress_passthrough_hosts,
            egress_max_bytes,
            registry_domain,
            marketplace_domain,
            connector_client_id_flags,
        }
    }

    /// The connectors' client ids from the environment and `--connector-client-id`. `vamd`
    /// refuses to start when this fails.
    pub fn connector_config(&self) -> Result<crate::connectors::ConnectorConfig, String> {
        crate::connectors::config_from(&|k| env::var(k).ok(), &self.connector_client_id_flags)
    }

    /// Validated registry and marketplace domains. `vamd` refuses to start
    /// when this fails.
    pub fn registry_domains(
        &self,
    ) -> Result<vam_spec::RegistryDomains, vam_spec::RegistryDomainError> {
        build_registry_domains(&self.registry_domain, &self.marketplace_domain)
    }

    pub fn egress_config(&self) -> EgressConfig {
        EgressConfig {
            cloudevents_url: self.cloudevents_url.clone(),
            spool_dir: self.spool_dir.clone(),
            mirror_otlp: self.cloudevents_mirror_otlp,
            otlp_enabled: self.otlp_enabled,
            openlineage_url: self.openlineage_url.clone(),
        }
    }

    pub fn otlp_config(&self) -> OtlpConfig {
        OtlpConfig {
            enabled: self.otlp_enabled,
            endpoint: self.otlp_endpoint.clone(),
            policy: TailSamplingPolicy {
                success_sample_rate: self.otlp_success_sample_rate,
                slow_trace_threshold_ms: self.otlp_slow_ms,
            },
            tls: self.otlp_tls.clone(),
            disabled_reason: self.otlp_disabled_reason.clone(),
        }
    }

    /// Validated tenant id. Invalid values (including `../`) are errors.
    pub fn tenant(&self) -> Result<TenantId, vam_telemetry::VamTelemetryError> {
        TenantId::new(self.tenant_id.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_node_names_every_missing_variable() {
        let none = |_: &str| None;
        assert_eq!(
            missing_vm_node_vars(none, true),
            ["VAMD_FIRECRACKER_BIN", "VAM_ROOTFS_PATH", "VAMD_JAILER_BIN"]
        );
        let msg = vm_node_config_error(&missing_vm_node_vars(none, true));
        for v in ["VAMD_FIRECRACKER_BIN", "VAM_ROOTFS_PATH", "VAMD_JAILER_BIN"] {
            assert!(msg.contains(v), "{msg}");
        }
        // The jailer binary is not needed with the jailer off.
        assert_eq!(
            missing_vm_node_vars(none, false),
            ["VAMD_FIRECRACKER_BIN", "VAM_ROOTFS_PATH"]
        );
        // Each variable alone, and a blank value counts as missing.
        for (set, gone) in [
            ("VAMD_FIRECRACKER_BIN", "VAM_ROOTFS_PATH"),
            ("VAM_ROOTFS_PATH", "VAMD_FIRECRACKER_BIN"),
        ] {
            let got = missing_vm_node_vars(|k| (k == set).then(|| "/x".to_string()), false);
            assert!(got.contains(&gone) && !got.contains(&set), "{got:?}");
        }
        let blank = missing_vm_node_vars(|_| Some("  ".to_string()), true);
        assert_eq!(blank.len(), 3);
        let all = missing_vm_node_vars(|_| Some("/x".to_string()), true);
        assert!(all.is_empty());
    }

    #[test]
    fn session_idle_ttl_default_off_and_invalid() {
        assert_eq!(parse_session_idle_ttl(None), (Some(86_400), None));
        assert_eq!(parse_session_idle_ttl(Some(" ")), (Some(86_400), None));
        assert_eq!(parse_session_idle_ttl(Some("3600")), (Some(3600), None));
        let (ttl, warn) = parse_session_idle_ttl(Some("off"));
        assert_eq!(ttl, None);
        assert!(warn.is_some());
        for bad in ["0", "-5", "soon", "1.5"] {
            let (ttl, warn) = parse_session_idle_ttl(Some(bad));
            assert_eq!(ttl, Some(86_400), "{bad}");
            assert!(warn.unwrap().contains(bad), "{bad}");
        }
    }

    #[test]
    fn rate_limits_are_on_by_default_and_off_only_when_asked() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let d = parse_rate_limits(env(&[]));
        assert!(d.enabled);
        assert_eq!((d.key_rps, d.key_burst), (30, 100));
        assert_eq!((d.create_rps, d.create_burst), (2, 20));
        for off in ["off", "OFF", "0", "false", "no"] {
            let pairs: &'static [(&str, &str)] =
                Box::leak(vec![(ENV_RATE_LIMIT, off)].into_boxed_slice());
            assert!(!parse_rate_limits(env(pairs)).enabled, "{off}");
        }
        // A typo does not switch limits off.
        assert!(parse_rate_limits(env(&[(ENV_RATE_LIMIT, "flase")])).enabled);
        let t = parse_rate_limits(env(&[
            (ENV_RATE_LIMIT_RPS, "50"),
            (ENV_RATE_LIMIT_BURST, "nope"),
            (ENV_RATE_LIMIT_CREATE_RPS, "0"),
        ]));
        assert_eq!((t.key_rps, t.key_burst, t.create_rps), (50, 100, 2));
    }

    #[test]
    fn insecure_dev_is_off_unless_asked() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(!parse_insecure_dev(None, &args(&["vamd"])));
        assert!(!parse_insecure_dev(Some("0"), &args(&["vamd"])));
        assert!(!parse_insecure_dev(Some(""), &args(&["vamd"])));
        assert!(parse_insecure_dev(Some("1"), &args(&["vamd"])));
        assert!(parse_insecure_dev(Some("TRUE"), &args(&["vamd"])));
        assert!(parse_insecure_dev(None, &args(&["vamd", "--insecure-dev"])));
        // The program name alone is not a flag.
        assert!(!parse_insecure_dev(None, &args(&["--insecure-dev"])));
    }

    #[test]
    fn the_jailer_is_on_by_default_and_only_an_explicit_opt_out_turns_it_off() {
        assert!(parse_use_jailer(None));
        assert!(parse_use_jailer(Some("")));
        assert!(parse_use_jailer(Some("1")));
        assert!(parse_use_jailer(Some("true")));
        for off in ["0", "false", "FALSE", " off ", "no"] {
            assert!(!parse_use_jailer(Some(off)), "{off}");
        }
        // The flags beat the environment, last one wins; the program name is not a flag.
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(!resolve_use_jailer(None, &args(&["vamd", "--no-jailer"])));
        assert!(!resolve_use_jailer(
            Some("1"),
            &args(&["vamd", "--no-jailer"])
        ));
        assert!(resolve_use_jailer(
            Some("0"),
            &args(&["vamd", "--use-jailer"])
        ));
        assert!(resolve_use_jailer(
            None,
            &args(&["vamd", "--no-jailer", "--use-jailer"])
        ));
        assert!(resolve_use_jailer(None, &args(&["--no-jailer"])));
        // A typo keeps the confinement on, and is reported.
        assert!(parse_use_jailer(Some("flase")));
        assert_eq!(
            unrecognised_use_jailer(Some("flase")).as_deref(),
            Some("flase")
        );
        assert_eq!(unrecognised_use_jailer(Some("0")), None);
        assert_eq!(unrecognised_use_jailer(None), None);
    }

    #[test]
    fn jailer_ids_default_to_a_dedicated_unprivileged_id() {
        assert_eq!(parse_jailer_id(None, 65400), 65400);
        assert_eq!(parse_jailer_id(Some("1234"), 65400), 1234);
        assert_eq!(parse_jailer_id(Some("nope"), 65400), 65400);
        // 0 is parsed, then refused by the preflight (see vam_core::jail).
        assert_eq!(parse_jailer_id(Some("0"), 65400), 0);
    }

    #[test]
    fn tls_sans_split_on_commas() {
        assert_eq!(
            parse_tls_sans(" a.example , 10.0.0.5,,"),
            vec!["a.example".to_string(), "10.0.0.5".to_string()]
        );
        assert!(parse_tls_sans("").is_empty());
    }

    #[test]
    fn default_telemetry_listen_is_localhost_9090() {
        assert_eq!(
            default_telemetry_listen(),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 9090))
        );
    }

    #[test]
    fn otlp_defaults_match_telemetry_policy() {
        let policy = TailSamplingPolicy::default();
        assert!(parse_otlp_enabled(None));
        assert_eq!(DEFAULT_OTLP_ENDPOINT, "http://127.0.0.1:4318");
        assert_eq!(policy.success_sample_rate, 0.05);
        assert_eq!(policy.slow_trace_threshold_ms, 1000);
    }

    #[test]
    fn registry_domains_default_to_local_and_the_squrell_marketplace() {
        let domains = build_registry_domains("", "").unwrap();
        assert_eq!(domains.registry_domain(), "local.vam.dev");
        assert_eq!(domains.marketplace_domain(), "hub.squrell.cloud");
        // The variable still overrides it.
        let staging = build_registry_domains("", "hub.staging.example").unwrap();
        assert_eq!(staging.marketplace_domain(), "hub.staging.example");
    }

    #[test]
    fn registry_domains_accept_valid_values() {
        let domains = build_registry_domains("vam.squrell.dev", "hub.example.test").unwrap();
        assert_eq!(domains.registry_domain(), "vam.squrell.dev");
        assert_eq!(domains.marketplace_domain(), "hub.example.test");
    }

    #[test]
    fn registry_domains_refuse_invalid_values() {
        assert!(build_registry_domains("Vam.Squrell.dev", "").is_err());
        assert!(build_registry_domains("vam.squrell.dev", "a/b").is_err());
        assert!(build_registry_domains("a.dev", "a.dev").is_err());
    }

    #[test]
    fn cloudevents_mirror_defaults_off() {
        assert!(!parse_flag_default_false(None));
        assert!(!parse_flag_default_false(Some("0")));
        assert!(parse_flag_default_false(Some("1")));
    }

    #[test]
    fn tool_hitl_timeout_parser_uses_one_hour_default() {
        assert_eq!(
            parse_tool_hitl_timeout_secs(None),
            crate::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS
        );
        assert_eq!(parse_tool_hitl_timeout_secs(Some("90")), 90);
        assert_eq!(
            parse_tool_hitl_timeout_secs(Some("0")),
            crate::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS
        );
        assert_eq!(
            parse_tool_hitl_timeout_secs(Some("not-a-number")),
            crate::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS
        );
        assert_eq!(
            parse_tool_hitl_timeout_secs(Some("99999999")),
            crate::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS
        );
    }

    #[test]
    fn egress_max_bytes_defaults_to_one_gib_and_zero_disables() {
        assert_eq!(parse_egress_max_bytes(None), 1024 * 1024 * 1024);
        assert_eq!(parse_egress_max_bytes(Some("")), DEFAULT_EGRESS_MAX_BYTES);
        assert_eq!(parse_egress_max_bytes(Some("  ")), DEFAULT_EGRESS_MAX_BYTES);
        assert_eq!(parse_egress_max_bytes(Some("0")), 0);
        assert_eq!(parse_egress_max_bytes(Some("4096")), 4096);
        assert_eq!(parse_egress_max_bytes(Some("-1")), DEFAULT_EGRESS_MAX_BYTES);
        assert_eq!(
            parse_egress_max_bytes(Some("lots")),
            DEFAULT_EGRESS_MAX_BYTES
        );
    }

    #[test]
    fn passthrough_hosts_default_empty_and_normalize() {
        assert!(parse_egress_passthrough_hosts("").is_empty());
        assert!(parse_egress_passthrough_hosts(" , ").is_empty());
        assert_eq!(
            parse_egress_passthrough_hosts("API.Example.com., api.example.com, pinned.test"),
            vec!["api.example.com".to_string(), "pinned.test".to_string()]
        );
    }

    fn argv(a: &[&str]) -> Vec<String> {
        std::iter::once("vamd")
            .chain(a.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn root_is_refused_everywhere_but_preflight() {
        assert!(one_shot_flag_error(&argv(&["--preflight", "--root", "/t"])).is_none());
        assert!(one_shot_flag_error(&argv(&["--init"])).is_none());
        assert!(one_shot_flag_error(&argv(&[])).is_none());
        for bad in [
            argv(&["--root", "/t"]),
            argv(&["--init", "--root", "/t"]),
            argv(&["--db-path", "/x", "--root", "/t"]),
        ] {
            let e = one_shot_flag_error(&bad).unwrap_or_default();
            assert!(e.contains("only valid with --preflight"), "{bad:?}: {e}");
        }
        assert!(one_shot_flag_error(&argv(&["--preflight", "--init"])).is_some());
    }

    #[test]
    fn the_logged_config_never_shows_the_bootstrap_key() {
        let mut c = DaemonConfig::parse();
        c.bootstrap_api_key = Some("vam_sk_secret-value".to_string());
        let shown = format!("{:?}", c.redacted());
        assert!(!shown.contains("secret-value"), "{shown}");
        assert!(shown.contains("<redacted>"));
    }
}
