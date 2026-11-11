//! Process-guest mode (cargo feature `process-guest`, TEST ONLY).
//!
//! In this mode vamd runs each session's guest as an ordinary host process
//! (`vam_core::ProcessVmm`) with NO isolation. That is only acceptable on a
//! node that cannot reach a real model or be reached from the network, so
//! everything here is a refusal:
//!
//! - startup ([`validate`], called by vamd before it opens any state):
//!   needs `VAMD_E2E_MOCK_SCRIPTS`; loopback-only HTTP and telemetry listeners;
//!   no provider-key import (`VAMD_IMPORT_ENV_SECRETS` unset); snapshots off;
//!   no jailer; an absolute guest binary; no unknown `VAMD_PROCESS_*` variable;
//! - runs: [`require_mock_model`] refuses any resolved model but `mock-model`;
//! - the inference broker is pinned to `mock-model` as well ([`ONLY_MODEL`]).
//!
//! The mode is selected by `VAMD_PROCESS_GUEST_BIN`. A vamd built without the
//! feature does not contain any of this: it only rejects `VAMD_PROCESS_*`.

use crate::allowed_models::MOCK_MODEL;
use crate::error::OrchestratorError;
use crate::VmmFactory;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

pub const ENV_GUEST_BIN: &str = "VAMD_PROCESS_GUEST_BIN";
/// Test-only seam for connectors (CN-1.2): maps the Entra authority host and the Linear MCP URL
/// to runner-hosted loopback mocks. A JSON object `{"microsoft-login": "http://127.0.0.1:P",
/// "linear-server": "http://127.0.0.1:Q/mcp", "microsoft-graph": "http://127.0.0.1:R"}`. It is read only under this feature, needs
/// `VAMD_PROCESS_GUEST_BIN`, and a default `vamd` refuses it like every `VAMD_PROCESS_*`.
pub const ENV_CONNECTOR_ENDPOINTS: &str = "VAMD_PROCESS_CONNECTOR_ENDPOINTS";
/// Test-only seam for marketplaces (CN-4.4): maps the https URL of a marketplace to a local bare
/// repository, so the runner's `file://` repos stand in for GitHub. A JSON object
/// `{"https://github.com/squrell-inc/vam-marketplace": "file:///tmp/run/marketplaces/m/bare.git"}`.
/// It needs `VAMD_PROCESS_GUEST_BIN`; a default `vamd` refuses it like every `VAMD_PROCESS_*` and has
/// no `file://` transport at all.
pub const ENV_MARKETPLACE_URLS: &str = "VAMD_PROCESS_MARKETPLACE_URLS";
const ENV_PREFIX: &str = "VAMD_PROCESS_";
const ENV_MOCK_SCRIPTS: &str = "VAMD_E2E_MOCK_SCRIPTS";
const ENV_IMPORT_SECRETS: &str = "VAMD_IMPORT_ENV_SECRETS";
const ENV_SNAPSHOT_RESTORE: &str = "VAM_SNAPSHOT_RESTORE";
const ENV_USE_JAILER: &str = "VAMD_USE_JAILER";
const ENV_LISTEN: &str = "VAMD_LISTEN";
const ENV_TELEMETRY_LISTEN: &str = "VAMD_TELEMETRY_LISTEN";

/// The only model a process-guest node serves.
pub const ONLY_MODEL: &str = MOCK_MODEL;

/// Everything process mode needs after the checks passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessMode {
    pub guest_bin: PathBuf,
    /// Where `ProcessVmm` keeps its per-guest state (pid file, log).
    pub state_root: PathBuf,
    /// Loopback stand-ins for the connectors' provider hosts (empty unless the seam is set).
    pub connector_endpoints: ConnectorEndpoints,
    /// Canonical https marketplace URL -> `file://` bare repository (empty unless the seam is set).
    pub marketplace_urls: std::collections::BTreeMap<String, String>,
}

/// Parses the marketplace seam: each key a canonical marketplace URL, each value an absolute
/// `file://` URL with no host, query or fragment.
pub fn parse_marketplace_urls(
    raw: &str,
) -> Result<std::collections::BTreeMap<String, String>, Refusal> {
    let bad = |why: String| Refusal::MarketplaceUrls(why);
    let map: std::collections::BTreeMap<String, String> =
        serde_json::from_str(raw).map_err(|_| bad("not a JSON object of strings".into()))?;
    if map.len() > 16 {
        return Err(bad("at most 16 entries".into()));
    }
    for (k, v) in &map {
        let checked =
            crate::marketplace::url::check_url(k).map_err(|e| bad(format!("{k:?}: {e}")))?;
        if &checked.canonical != k {
            return Err(bad(format!(
                "{k:?}: write the canonical form {:?}",
                checked.canonical
            )));
        }
        let f = url::Url::parse(v).map_err(|_| bad(format!("{v:?}: not a URL")))?;
        if f.scheme() != "file"
            || !f.host_str().unwrap_or("").is_empty()
            || f.query().is_some()
            || f.fragment().is_some()
            || !f.path().starts_with('/')
            || v.contains("..")
            || v.contains('%')
        {
            return Err(bad(format!("{v:?}: must be file:///absolute/path")));
        }
    }
    Ok(map)
}

/// Where the provider hosts are served from in a test node. Every URL is loopback `http`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectorEndpoints {
    /// Stands in for `https://login.microsoftonline.com` (the path is kept).
    pub microsoft_login: Option<url::Url>,
    /// Stands in for the Linear definition's MCP URL.
    pub linear_server: Option<url::Url>,
    /// Stands in for `https://graph.microsoft.com`: the egress gate dials this loopback http
    /// server for that one host (scheme, host and port only; the request path is kept).
    pub microsoft_graph: Option<url::Url>,
    /// Stands in for the exact https URL of an MCP server a connector from a marketplace names
    /// (`"mcp:<https url>": "<loopback http url>"`).
    pub mcp_servers: std::collections::BTreeMap<String, url::Url>,
}

fn loopback_http(raw: &str) -> Result<url::Url, Refusal> {
    let bad = |why: &str| Refusal::ConnectorEndpoints(format!("{raw:?}: {why}"));
    let u = url::Url::parse(raw).map_err(|_| bad("not a URL"))?;
    if u.scheme() != "http"
        || !matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(bad("must be a plain http loopback URL"));
    }
    Ok(u)
}

impl ConnectorEndpoints {
    /// Parses the seam's JSON. Unknown keys and non-loopback hosts are refused.
    pub fn parse(raw: &str) -> Result<Self, Refusal> {
        let map: std::collections::BTreeMap<String, String> = serde_json::from_str(raw)
            .map_err(|_| Refusal::ConnectorEndpoints("not a JSON object of strings".into()))?;
        let mut out = Self::default();
        for (k, v) in &map {
            match k.as_str() {
                "microsoft-login" => out.microsoft_login = Some(loopback_http(v)?),
                "linear-server" => out.linear_server = Some(loopback_http(v)?),
                "microsoft-graph" => {
                    let base = loopback_http(v)?;
                    if base.port().is_none() || !matches!(base.path(), "" | "/") {
                        return Err(Refusal::ConnectorEndpoints(format!(
                            "{v:?}: microsoft-graph must be http://127.0.0.1:<port> with no path"
                        )));
                    }
                    out.microsoft_graph = Some(base);
                }
                other => match other.strip_prefix("mcp:") {
                    Some(real) if real.starts_with("https://") && real.len() <= 512 => {
                        out.mcp_servers.insert(real.to_string(), loopback_http(v)?);
                    }
                    _ => {
                        return Err(Refusal::ConnectorEndpoints(format!(
                            "unknown key `{other}`"
                        )))
                    }
                },
            }
        }
        Ok(out)
    }

    /// The loopback URL for `url` when it is one of the two provider URLs the seam stands in
    /// for: the Linear definition's MCP URL (exact) or a path on the Entra authority host.
    /// Anything else is `None` and keeps the normal SSRF rules.
    pub fn map(&self, url: &str) -> Option<url::Url> {
        if let Some(linear) = &self.linear_server {
            let real = crate::connectors::builtin("linear").and_then(|c| c.mcp.map(|m| m.url));
            if real.as_deref() == Some(url) {
                return Some(linear.clone());
            }
        }
        if let Some(mock) = self.mcp_servers.get(url) {
            return Some(mock.clone());
        }
        if let Some(login) = &self.microsoft_login {
            let u = url::Url::parse(url).ok()?;
            if u.scheme() == "https" && u.host_str() == Some("login.microsoftonline.com") {
                let mut mapped = login.clone();
                mapped.set_path(u.path());
                mapped.set_query(u.query());
                return Some(mapped);
            }
        }
        None
    }
}

/// Why vamd refuses to start in process mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    MockScriptsMissing,
    NotLoopback { what: &'static str, addr: String },
    UnparsableListen { var: &'static str, value: String },
    ImportEnvSecrets,
    SnapshotRestoreOn,
    JailerOn,
    GuestBinEmpty,
    GuestBinNotAbsolute(String),
    GuestBinNotAFile(String),
    UnknownVariable(String),
    ConnectorEndpoints(String),
    ConnectorEndpointsNeedProcessMode,
    MarketplaceUrls(String),
    MarketplaceUrlsNeedProcessMode,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let why = match self {
            Self::MockScriptsMissing => {
                format!(
                    "{ENV_MOCK_SCRIPTS} is not set (process guests are for mock-model tests only)"
                )
            }
            Self::NotLoopback { what, addr } => {
                format!(
                    "{what} {addr} is not a loopback address (process guests have no isolation)"
                )
            }
            Self::UnparsableListen { var, value } => {
                format!("{var}={value:?} is not a socket address (vamd would silently ignore it)")
            }
            Self::ImportEnvSecrets => {
                format!(
                    "{ENV_IMPORT_SECRETS} is set (no provider key may enter a process-guest node)"
                )
            }
            Self::SnapshotRestoreOn => {
                format!("{ENV_SNAPSHOT_RESTORE} must be 0 (process guests have no snapshots)")
            }
            Self::JailerOn => format!("{ENV_USE_JAILER} is on (process guests have no jailer)"),
            Self::GuestBinEmpty => format!("{ENV_GUEST_BIN} is empty"),
            Self::GuestBinNotAbsolute(p) => {
                format!("{ENV_GUEST_BIN}={p:?} is not an absolute path")
            }
            Self::GuestBinNotAFile(p) => format!("{ENV_GUEST_BIN}={p:?} is not a file"),
            Self::UnknownVariable(v) => {
                format!(
                    "unknown variable {v} (only {ENV_GUEST_BIN}, {ENV_CONNECTOR_ENDPOINTS} and {ENV_MARKETPLACE_URLS} exist)"
                )
            }
            Self::ConnectorEndpoints(why) => format!("{ENV_CONNECTOR_ENDPOINTS}: {why}"),
            Self::ConnectorEndpointsNeedProcessMode => {
                format!("{ENV_CONNECTOR_ENDPOINTS} is set without {ENV_GUEST_BIN}")
            }
            Self::MarketplaceUrls(why) => format!("{ENV_MARKETPLACE_URLS}: {why}"),
            Self::MarketplaceUrlsNeedProcessMode => {
                format!("{ENV_MARKETPLACE_URLS} is set without {ENV_GUEST_BIN}")
            }
        };
        write!(f, "vamd: process-guest mode refused: {why}")
    }
}

fn truthy(v: &str) -> bool {
    v == "1" || v.eq_ignore_ascii_case("true")
}

/// Checks the environment of a vamd about to start. `get` reads one variable,
/// `names` lists the variable names present (to spot unknown `VAMD_PROCESS_*`),
/// `listen` and `telemetry_listen` are the addresses vamd resolved from its
/// config. Returns `Ok(None)` when process mode was not asked for.
pub fn validate(
    get: &dyn Fn(&str) -> Option<String>,
    names: &[String],
    listen: SocketAddr,
    telemetry_listen: SocketAddr,
    state_root: PathBuf,
) -> Result<Option<ProcessMode>, Refusal> {
    if let Some(unknown) = names.iter().find(|n| {
        n.starts_with(ENV_PREFIX)
            && n.as_str() != ENV_GUEST_BIN
            && n.as_str() != ENV_CONNECTOR_ENDPOINTS
            && n.as_str() != ENV_MARKETPLACE_URLS
    }) {
        return Err(Refusal::UnknownVariable(unknown.clone()));
    }
    let Some(bin) = get(ENV_GUEST_BIN) else {
        if get(ENV_CONNECTOR_ENDPOINTS).is_some() {
            return Err(Refusal::ConnectorEndpointsNeedProcessMode);
        }
        if get(ENV_MARKETPLACE_URLS).is_some() {
            return Err(Refusal::MarketplaceUrlsNeedProcessMode);
        }
        return Ok(None);
    };
    if bin.trim().is_empty() {
        return Err(Refusal::GuestBinEmpty);
    }
    let guest_bin = PathBuf::from(&bin);
    if !guest_bin.is_absolute() {
        return Err(Refusal::GuestBinNotAbsolute(bin));
    }
    if !guest_bin.is_file() {
        return Err(Refusal::GuestBinNotAFile(bin));
    }
    if get(ENV_MOCK_SCRIPTS)
        .map(|v| v.trim().is_empty())
        .unwrap_or(true)
    {
        return Err(Refusal::MockScriptsMissing);
    }
    for var in [ENV_LISTEN, ENV_TELEMETRY_LISTEN] {
        if let Some(v) = get(var) {
            if v.parse::<SocketAddr>().is_err() {
                return Err(Refusal::UnparsableListen { var, value: v });
            }
        }
    }
    for (what, addr) in [
        ("HTTP listener", listen),
        ("telemetry listener", telemetry_listen),
    ] {
        if !addr.ip().is_loopback() {
            return Err(Refusal::NotLoopback {
                what,
                addr: addr.to_string(),
            });
        }
    }
    if get(ENV_IMPORT_SECRETS).is_some() {
        return Err(Refusal::ImportEnvSecrets);
    }
    if get(ENV_SNAPSHOT_RESTORE).as_deref() != Some("0") {
        return Err(Refusal::SnapshotRestoreOn);
    }
    if get(ENV_USE_JAILER).map(|v| truthy(&v)).unwrap_or(false) {
        return Err(Refusal::JailerOn);
    }
    let connector_endpoints = match get(ENV_CONNECTOR_ENDPOINTS) {
        Some(raw) => ConnectorEndpoints::parse(&raw)?,
        None => ConnectorEndpoints::default(),
    };
    let marketplace_urls = match get(ENV_MARKETPLACE_URLS) {
        Some(raw) => parse_marketplace_urls(&raw)?,
        None => Default::default(),
    };
    Ok(Some(ProcessMode {
        guest_bin,
        state_root,
        connector_endpoints,
        marketplace_urls,
    }))
}

/// [`validate`] against the real process environment.
pub fn validate_env(
    listen: SocketAddr,
    telemetry_listen: SocketAddr,
    state_root: PathBuf,
) -> Result<Option<ProcessMode>, Refusal> {
    let names: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .collect();
    validate(
        &|k| std::env::var(k).ok(),
        &names,
        listen,
        telemetry_listen,
        state_root,
    )
}

static ACTIVE: OnceLock<ProcessMode> = OnceLock::new();

/// Turns process mode on for this vamd (once).
pub fn install(mode: ProcessMode) {
    let _ = ACTIVE.set(mode);
}

/// The loopback stand-in for a connector provider URL, in a node started with the seam.
/// `None` on every other node and for every other URL.
pub fn map_connector_url(url: &str) -> Option<url::Url> {
    ACTIVE.get()?.connector_endpoints.map(url)
}

/// The local bare repository that stands in for a marketplace URL, in a node started with the seam.
/// `None` on every other node and for every other URL (which then keep the https-only rules).
pub fn map_marketplace_url(url: &str) -> Option<String> {
    ACTIVE.get()?.marketplace_urls.get(url).cloned()
}

/// The loopback server that answers `graph.microsoft.com` in a node started with the seam.
pub fn graph_endpoint() -> Option<url::Url> {
    ACTIVE.get()?.connector_endpoints.microsoft_graph.clone()
}

pub fn is_active() -> bool {
    ACTIVE.get().is_some()
}

pub fn active() -> Option<&'static ProcessMode> {
    ACTIVE.get()
}

/// The run-time refusal: `model` is what the run resolved to.
pub fn check_model(active: bool, model: &str) -> Result<(), OrchestratorError> {
    if active && model.trim() != ONLY_MODEL {
        return Err(OrchestratorError::InvalidContract(format!(
            "this node runs process guests with no isolation and serves only '{ONLY_MODEL}', not '{}'",
            model.trim()
        )));
    }
    Ok(())
}

/// Refuses a run whose resolved model is not `mock-model` while process mode is on.
pub fn require_mock_model(model: &str) -> Result<(), OrchestratorError> {
    check_model(is_active(), model)
}

/// The WARN vamd logs on every boot in this mode.
pub fn banner(mode: &ProcessMode) -> String {
    format!(
        "PROCESS GUEST MODE: guests run as host processes from {} with NO isolation; test node only (mock-model, loopback)",
        mode.guest_bin.display()
    )
}

/// `VmmFactory` building `ProcessVmm`s.
pub fn vmm_factory(mode: &ProcessMode) -> VmmFactory {
    let bin = mode.guest_bin.clone();
    let root = mode.state_root.clone();
    Arc::new(move |vm_id, guest_cid| {
        Box::new(vam_core::ProcessVmm::new(
            vm_id.to_string(),
            bin.clone(),
            guest_cid,
            root.clone(),
        ))
    })
}

/// At startup, kills any guest a previous vamd left running for a session this
/// store knows, and removes its files. Only this store's sessions are touched.
pub fn sweep_known_sessions(
    store: &crate::workflow::SqliteWorkflowStore,
    mode: &ProcessMode,
) -> usize {
    let sessions = store.load_all_sessions().unwrap_or_default();
    for s in &sessions {
        vam_core::process_vmm::kill_and_sweep(&mode.state_root, &s.vm_id);
    }
    sessions.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Env {
        vars: HashMap<String, String>,
        _bin: tempfile::NamedTempFile,
    }

    impl Env {
        fn ok() -> Self {
            let bin = tempfile::NamedTempFile::new().unwrap();
            let mut vars = HashMap::new();
            vars.insert(ENV_GUEST_BIN.into(), bin.path().display().to_string());
            vars.insert(ENV_MOCK_SCRIPTS.into(), "/scripts".into());
            vars.insert(ENV_SNAPSHOT_RESTORE.into(), "0".into());
            Env { vars, _bin: bin }
        }
        fn set(mut self, k: &str, v: &str) -> Self {
            self.vars.insert(k.into(), v.into());
            self
        }
        fn unset(mut self, k: &str) -> Self {
            self.vars.remove(k);
            self
        }
        fn run(&self, listen: &str, telemetry: &str) -> Result<Option<ProcessMode>, Refusal> {
            let names: Vec<String> = self.vars.keys().cloned().collect();
            validate(
                &|k| self.vars.get(k).cloned(),
                &names,
                listen.parse().unwrap(),
                telemetry.parse().unwrap(),
                PathBuf::from("/state"),
            )
        }
        fn go(&self) -> Result<Option<ProcessMode>, Refusal> {
            self.run("127.0.0.1:8080", "127.0.0.1:9090")
        }
    }

    #[test]
    fn a_complete_environment_is_accepted() {
        let m = Env::ok().go().unwrap().expect("process mode");
        assert_eq!(m.state_root, PathBuf::from("/state"));
        assert!(
            Env::ok().run("[::1]:8080", "[::1]:9090").is_ok(),
            "ipv6 loopback"
        );
    }

    #[test]
    fn not_asked_for_is_not_process_mode() {
        assert_eq!(Env::ok().unset(ENV_GUEST_BIN).go().unwrap(), None);
    }

    #[test]
    fn refuses_without_mock_scripts() {
        assert_eq!(
            Env::ok().unset(ENV_MOCK_SCRIPTS).go().unwrap_err(),
            Refusal::MockScriptsMissing
        );
        assert_eq!(
            Env::ok().set(ENV_MOCK_SCRIPTS, "  ").go().unwrap_err(),
            Refusal::MockScriptsMissing
        );
    }

    #[test]
    fn refuses_a_non_loopback_listener() {
        for (l, t) in [
            ("0.0.0.0:8080", "127.0.0.1:9090"),
            ("10.1.2.3:8080", "127.0.0.1:9090"),
            ("127.0.0.1:8080", "0.0.0.0:9090"),
        ] {
            assert!(
                matches!(
                    Env::ok().run(l, t).unwrap_err(),
                    Refusal::NotLoopback { .. }
                ),
                "{l} {t}"
            );
        }
    }

    #[test]
    fn refuses_an_unparsable_listen_address() {
        // vamd's own config parser would drop it and keep the default.
        let e = Env::ok().set(ENV_LISTEN, "localhost:80").go().unwrap_err();
        assert!(matches!(
            e,
            Refusal::UnparsableListen {
                var: "VAMD_LISTEN",
                ..
            }
        ));
        let e = Env::ok()
            .set(ENV_TELEMETRY_LISTEN, "nope")
            .go()
            .unwrap_err();
        assert!(matches!(e, Refusal::UnparsableListen { .. }));
    }

    #[test]
    fn refuses_secret_import_at_any_value() {
        for v in ["1", "true", "0", ""] {
            assert_eq!(
                Env::ok().set(ENV_IMPORT_SECRETS, v).go().unwrap_err(),
                Refusal::ImportEnvSecrets,
                "{v:?}"
            );
        }
    }

    #[test]
    fn refuses_snapshots_and_jailer() {
        assert_eq!(
            Env::ok().unset(ENV_SNAPSHOT_RESTORE).go().unwrap_err(),
            Refusal::SnapshotRestoreOn
        );
        assert_eq!(
            Env::ok().set(ENV_SNAPSHOT_RESTORE, "1").go().unwrap_err(),
            Refusal::SnapshotRestoreOn
        );
        assert_eq!(
            Env::ok().set(ENV_USE_JAILER, "true").go().unwrap_err(),
            Refusal::JailerOn
        );
        assert_eq!(
            Env::ok().set(ENV_USE_JAILER, "1").go().unwrap_err(),
            Refusal::JailerOn
        );
        assert!(Env::ok().set(ENV_USE_JAILER, "false").go().is_ok());
    }

    #[test]
    fn refuses_a_bad_guest_binary() {
        assert_eq!(
            Env::ok().set(ENV_GUEST_BIN, " ").go().unwrap_err(),
            Refusal::GuestBinEmpty
        );
        assert!(matches!(
            Env::ok()
                .set(ENV_GUEST_BIN, "vam-runtime")
                .go()
                .unwrap_err(),
            Refusal::GuestBinNotAbsolute(_)
        ));
        assert!(matches!(
            Env::ok()
                .set(ENV_GUEST_BIN, "/nonexistent/vam-runtime")
                .go()
                .unwrap_err(),
            Refusal::GuestBinNotAFile(_)
        ));
    }

    #[test]
    fn refuses_unknown_process_variables() {
        assert!(matches!(
            Env::ok()
                .set("VAMD_PROCESS_GUEST_BINN", "x")
                .go()
                .unwrap_err(),
            Refusal::UnknownVariable(_)
        ));
        assert!(matches!(
            Env::ok()
                .unset(ENV_GUEST_BIN)
                .set("VAMD_PROCESS_OTHER", "x")
                .go()
                .unwrap_err(),
            Refusal::UnknownVariable(_)
        ));
    }

    #[test]
    fn runs_with_another_model_are_refused_in_process_mode() {
        assert!(check_model(true, "mock-model").is_ok());
        assert!(check_model(true, " mock-model ").is_ok());
        for m in [
            "openai/gpt-4o",
            "anthropic/claude-sonnet-4",
            "mock-model-2",
            "",
        ] {
            let err = check_model(true, m).unwrap_err();
            assert!(matches!(err, OrchestratorError::InvalidContract(_)), "{m}");
        }
        // not in process mode: untouched
        assert!(check_model(false, "openai/gpt-4o").is_ok());
    }

    #[test]
    fn the_graph_key_is_a_loopback_http_root_and_maps_no_url() {
        let seam = r#"{"microsoft-graph":"http://127.0.0.1:41003"}"#;
        let m = Env::ok()
            .set(ENV_CONNECTOR_ENDPOINTS, seam)
            .go()
            .unwrap()
            .unwrap();
        let e = &m.connector_endpoints;
        assert_eq!(
            e.microsoft_graph.as_ref().map(url::Url::as_str),
            Some("http://127.0.0.1:41003/")
        );
        // It is served by the egress gate, not by the URL mapper the MCP client and refresh use.
        assert!(e.map("https://graph.microsoft.com/v1.0/me").is_none());
        for bad in [
            r#"{"microsoft-graph":"https://127.0.0.1:1"}"#,
            r#"{"microsoft-graph":"http://graph.microsoft.com:1"}"#,
            r#"{"microsoft-graph":"http://10.0.0.5:80"}"#,
            r#"{"microsoft-graph":"http://127.0.0.1"}"#,
            r#"{"microsoft-graph":"http://127.0.0.1:1/v1.0"}"#,
            r#"{"microsoft-graph":"http://u:p@127.0.0.1:1"}"#,
        ] {
            assert!(
                matches!(
                    Env::ok()
                        .set(ENV_CONNECTOR_ENDPOINTS, bad)
                        .go()
                        .unwrap_err(),
                    Refusal::ConnectorEndpoints(_)
                ),
                "{bad}"
            );
        }
        // Without process mode the variable is still refused as a whole.
        assert_eq!(
            Env::ok()
                .unset(ENV_GUEST_BIN)
                .set(ENV_CONNECTOR_ENDPOINTS, seam)
                .go()
                .unwrap_err(),
            Refusal::ConnectorEndpointsNeedProcessMode
        );
    }

    const SEAM: &str = r#"{"microsoft-login":"http://127.0.0.1:41001","linear-server":"http://127.0.0.1:41002/mcp"}"#;

    #[test]
    fn an_mcp_server_of_a_marketplace_connector_maps_by_its_exact_https_url() {
        let seam = r#"{"mcp:https://mcp.bugwatch.example/mcp":"http://127.0.0.1:41002/mcp"}"#;
        let m = Env::ok()
            .set(ENV_CONNECTOR_ENDPOINTS, seam)
            .go()
            .unwrap()
            .unwrap();
        let e = &m.connector_endpoints;
        assert_eq!(
            e.map("https://mcp.bugwatch.example/mcp").unwrap().as_str(),
            "http://127.0.0.1:41002/mcp"
        );
        for other in [
            "https://mcp.bugwatch.example/other",
            "https://mcp.bugwatch.example/mcp/",
            "https://mcp2.bugwatch.example/mcp",
        ] {
            assert!(e.map(other).is_none(), "{other}");
        }
        for bad in [
            r#"{"mcp:http://mcp.bugwatch.example/mcp":"http://127.0.0.1:1/mcp"}"#,
            r#"{"mcp:https://mcp.bugwatch.example/mcp":"https://127.0.0.1:1/mcp"}"#,
            r#"{"mcp:https://mcp.bugwatch.example/mcp":"http://10.0.0.5:1/mcp"}"#,
            r#"{"mcp:":"http://127.0.0.1:1/mcp"}"#,
        ] {
            assert!(
                matches!(
                    Env::ok()
                        .set(ENV_CONNECTOR_ENDPOINTS, bad)
                        .go()
                        .unwrap_err(),
                    Refusal::ConnectorEndpoints(_)
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn connector_endpoints_are_known_loopback_and_need_process_mode() {
        let m = Env::ok()
            .set(ENV_CONNECTOR_ENDPOINTS, SEAM)
            .go()
            .unwrap()
            .unwrap();
        let e = &m.connector_endpoints;
        // Linear's real MCP URL (exact) and the Entra authority are mapped, nothing else.
        assert_eq!(
            e.map("https://mcp.linear.app/mcp").unwrap().as_str(),
            "http://127.0.0.1:41002/mcp"
        );
        assert_eq!(
            e.map("https://login.microsoftonline.com/T/oauth2/v2.0/token")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:41001/T/oauth2/v2.0/token"
        );
        for other in [
            "https://mcp.linear.app/other",
            "https://mcp.example.com/mcp",
            "https://evil.test/T/oauth2/v2.0/token",
            "http://login.microsoftonline.com/T/oauth2/v2.0/token",
        ] {
            assert!(e.map(other).is_none(), "{other}");
        }
        // Not set: nothing is mapped.
        let none = Env::ok().go().unwrap().unwrap();
        assert!(none
            .connector_endpoints
            .map("https://mcp.linear.app/mcp")
            .is_none());
        // Without process mode the variable is refused, not ignored.
        assert_eq!(
            Env::ok()
                .unset(ENV_GUEST_BIN)
                .set(ENV_CONNECTOR_ENDPOINTS, SEAM)
                .go()
                .unwrap_err(),
            Refusal::ConnectorEndpointsNeedProcessMode
        );
        for bad in [
            "not json",
            r#"{"microsoft-login":"https://127.0.0.1:1"}"#,
            r#"{"microsoft-login":"http://login.microsoftonline.com"}"#,
            r#"{"linear-server":"http://10.0.0.5:80/mcp"}"#,
            r#"{"linear-server":"http://user:pw@127.0.0.1:80/mcp"}"#,
            r#"{"other":"http://127.0.0.1:1"}"#,
        ] {
            assert!(
                matches!(
                    Env::ok()
                        .set(ENV_CONNECTOR_ENDPOINTS, bad)
                        .go()
                        .unwrap_err(),
                    Refusal::ConnectorEndpoints(_)
                ),
                "{bad}"
            );
        }
    }

    const MKT_SEAM: &str = r#"{"https://github.com/squrell-inc/vam-marketplace":"file:///tmp/run/marketplaces/m/bare.git"}"#;

    #[test]
    fn marketplace_urls_map_a_canonical_https_url_to_a_local_repository() {
        let m = Env::ok()
            .set(ENV_MARKETPLACE_URLS, MKT_SEAM)
            .go()
            .unwrap()
            .unwrap();
        assert_eq!(
            m.marketplace_urls
                .get("https://github.com/squrell-inc/vam-marketplace")
                .map(String::as_str),
            Some("file:///tmp/run/marketplaces/m/bare.git")
        );
        // Not set: nothing is mapped.
        assert!(Env::ok().go().unwrap().unwrap().marketplace_urls.is_empty());
        // Without process mode the variable is refused, not ignored.
        assert_eq!(
            Env::ok()
                .unset(ENV_GUEST_BIN)
                .set(ENV_MARKETPLACE_URLS, MKT_SEAM)
                .go()
                .unwrap_err(),
            Refusal::MarketplaceUrlsNeedProcessMode
        );
        for bad in [
            "not json",
            r#"{"http://github.com/a/b":"file:///x/bare.git"}"#,
            r#"{"https://github.com/a/b/":"file:///x/bare.git"}"#,
            r#"{"https://127.0.0.1/a/b":"file:///x/bare.git"}"#,
            r#"{"https://github.com/a/b":"https://github.com/a/b"}"#,
            r#"{"https://github.com/a/b":"file://host/x/bare.git"}"#,
            r#"{"https://github.com/a/b":"file:///x/../etc/bare.git"}"#,
            r#"{"https://github.com/a/b":"/x/bare.git"}"#,
        ] {
            assert!(
                matches!(
                    Env::ok().set(ENV_MARKETPLACE_URLS, bad).go().unwrap_err(),
                    Refusal::MarketplaceUrls(_)
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn banner_names_the_mode_and_the_binary() {
        let b = banner(&ProcessMode {
            guest_bin: "/x/vam-runtime".into(),
            state_root: "/s".into(),
            connector_endpoints: ConnectorEndpoints::default(),
            marketplace_urls: Default::default(),
        });
        assert!(
            b.starts_with("PROCESS GUEST MODE")
                && b.contains("NO isolation")
                && b.contains("/x/vam-runtime")
        );
    }

    #[test]
    fn guest_mode_defaults_to_vm() {
        // The test binary never installs process mode.
        assert_eq!(crate::guest_mode(), "vm");
    }
}
