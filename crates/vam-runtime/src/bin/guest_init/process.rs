//! Process-guest init (cargo feature `process-guest`, TEST ONLY).
//!
//! The guest runs as an ordinary host process instead of a microVM's PID 1, so
//! nothing here may act on the machine: no mounts, no `ifconfig`, no
//! `kill(-1)`, no `reboot`, no `umount`, no `clock_settime`, no `/dev/urandom`
//! writes, and no real vsock. The PID-1 versions of those do not exist in this
//! build (`pid1.rs` is compiled out). The only configuration source is env set
//! by the host-side `ProcessVmm`; a missing or invalid value refuses to start.

#[cfg(not(unix))]
compile_error!("the process-guest feature needs a unix host");

use std::path::PathBuf;
use std::sync::OnceLock;
use vam_runtime::EgressEndpoint;
use vam_vsock::guest::set_transport;
use vam_vsock::GuestTransport;

/// Which init this binary was built with.
#[allow(dead_code)]
pub const MODE: &str = "process";

/// Host-side vsock UDS base (`<base>_<port>` is where the host listens).
pub const ENV_UDS: &str = "VAM_GUEST_VSOCK_UDS";
pub const ENV_INSTANCE: &str = "VAM_INSTANCE_ID";
pub const ENV_CID: &str = "VAM_GUEST_CID";
/// Per-guest egress proxy unix socket (absolute). Unset = no egress proxy.
pub const ENV_EGRESS_SOCK: &str = "VAM_EGRESS_PROXY_SOCK";
/// Optional loopback `ip:port` for the proxy (port 0 = free port).
pub const ENV_EGRESS_ADDR: &str = "VAM_EGRESS_PROXY_ADDR";

/// Exit codes the host side can tell apart.
const EXIT_OK: i32 = 0;
const EXIT_FAILED: i32 = 1;
const EXIT_REFUSED: i32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessConfig {
    pub uds_base: PathBuf,
    pub instance_id: String,
    pub cid: u32,
    pub egress: Option<EgressEndpoint>,
}

impl ProcessConfig {
    /// Builds the config from a variable lookup. No defaults: a guest that is
    /// not told where its host is must not guess.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let required = |name: &str| {
            get(name)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("{name} is required in process-guest mode"))
        };
        let uds_base = PathBuf::from(required(ENV_UDS)?);
        if !uds_base.is_absolute() {
            return Err(format!("{ENV_UDS} must be an absolute path"));
        }
        let instance_id = required(ENV_INSTANCE)?;
        let cid = required(ENV_CID)?
            .parse::<u32>()
            .map_err(|e| format!("{ENV_CID} is not a u32: {e}"))?;
        let sock = get(ENV_EGRESS_SOCK).filter(|v| !v.is_empty());
        let addr = get(ENV_EGRESS_ADDR).filter(|v| !v.is_empty());
        let egress = match (sock, addr) {
            (Some(sock), addr) => Some(EgressEndpoint::per_guest(&sock, addr.as_deref())?),
            (None, Some(_)) => {
                return Err(format!("{ENV_EGRESS_ADDR} needs {ENV_EGRESS_SOCK}"));
            }
            (None, None) => None,
        };
        Ok(Self {
            uds_base,
            instance_id,
            cid,
            egress,
        })
    }
}

static CONFIG: OnceLock<ProcessConfig> = OnceLock::new();

fn config() -> &'static ProcessConfig {
    CONFIG.get().expect("init::startup() runs before any use")
}

fn refuse(msg: &str) -> ! {
    eprintln!("[guest-agent] process-guest refused to start: {msg}");
    std::process::exit(EXIT_REFUSED);
}

/// Validates the environment and selects the Unix-socket transport. Exits 2
/// without dialing anything when the config is missing or invalid, or when the
/// binary is PID 1 (it would be running as a rootfs init without one).
pub fn startup() {
    if std::process::id() == 1 {
        refuse("process-guest build must not run as PID 1");
    }
    eprintln!("[guest-agent] process-guest mode: no mounts, no PID-1 actions, no real vsock.");
    let cfg = match ProcessConfig::from_lookup(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => refuse(&e),
    };
    if !cfg.uds_base.parent().is_some_and(|p| p.is_dir()) {
        refuse(&format!(
            "the directory of {} does not exist",
            cfg.uds_base.display()
        ));
    }
    set_transport(GuestTransport::Uds {
        base: cfg.uds_base.clone(),
    });
    let _ = CONFIG.set(cfg);
    register_signal_handlers();
}

/// SIGTERM/SIGINT end this process only: kill the tool process groups it
/// started (never `kill(-1)`), remove its sockets, exit 0.
fn register_signal_handlers() {
    use tokio::signal::unix::{signal, SignalKind};
    tokio::spawn(async {
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            eprintln!("[guest-agent] could not register signal handlers");
            return;
        };
        tokio::select! {
            _ = term.recv() => eprintln!("[guest-agent] Intercepted SIGTERM."),
            _ = int.recv() => eprintln!("[guest-agent] Intercepted SIGINT."),
        }
        halt("Graceful signal-initiated shutdown complete".to_string());
    });
}

/// A process guest is never the golden bake VM.
pub fn golden_bake() -> bool {
    false
}

pub fn instance_id() -> String {
    config().instance_id.clone()
}

pub fn local_cid() -> u32 {
    config().cid
}

/// True when the host configured a per-guest egress endpoint.
pub fn egress_enabled() -> bool {
    config().egress.is_some()
}

/// Serves the proxy on this guest's own loopback/unix endpoint. There is no
/// fixed port and no wildcard fallback.
pub async fn start_egress_proxy<S>(stream: S) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let Some(endpoint) = config().egress.as_ref() else {
        return Err("no egress endpoint configured".to_string());
    };
    let bound = vam_runtime::start_guest_proxy_with(endpoint, stream).await?;
    if let Some(addr) = bound.tcp {
        vam_runtime::advertise_proxy_url(format!("http://{addr}"));
    }
    Ok(())
}

/// Ends this process. Never touches the machine: tool process groups started
/// by this guest are killed, this guest's sockets are removed, then it exits
/// (0 for a requested shutdown or signal, 1 otherwise).
pub fn halt(reason: String) -> ! {
    eprintln!("[guest-agent] HALTING (process-guest). Reason: {}", reason);
    vam_runtime::cancel_all_active_leases();
    if let Some(cfg) = CONFIG.get() {
        let _ = std::fs::remove_file(&cfg.uds_base);
        if let Some(ep) = &cfg.egress {
            let _ = std::fs::remove_file(&ep.unix);
        }
    }
    let code = if reason.starts_with("Graceful") {
        EXIT_OK
    } else {
        EXIT_FAILED
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    const BASE: [(&str, &str); 3] = [
        (ENV_UDS, "/tmp/vam-vsock-a.socket"),
        (ENV_INSTANCE, "urn:vam:instance:a"),
        (ENV_CID, "7"),
    ];

    #[test]
    fn refuses_without_each_required_variable() {
        for skip in [ENV_UDS, ENV_INSTANCE, ENV_CID] {
            let pairs: Vec<_> = BASE.iter().filter(|(k, _)| *k != skip).cloned().collect();
            let err = ProcessConfig::from_lookup(lookup(&pairs)).unwrap_err();
            assert!(err.contains(skip), "{err}");
        }
        assert!(ProcessConfig::from_lookup(lookup(&[])).is_err());
    }

    #[test]
    fn refuses_a_relative_base_and_a_bad_cid() {
        let rel = [(ENV_UDS, "rel.socket"), BASE[1], BASE[2]];
        assert!(ProcessConfig::from_lookup(lookup(&rel)).is_err());
        let bad = [BASE[0], BASE[1], (ENV_CID, "-1")];
        assert!(ProcessConfig::from_lookup(lookup(&bad)).is_err());
    }

    #[test]
    fn egress_is_off_unless_configured_and_never_wildcard() {
        let cfg = ProcessConfig::from_lookup(lookup(&BASE)).unwrap();
        assert_eq!(cfg.egress, None);
        let mut with = BASE.to_vec();
        with.push((ENV_EGRESS_SOCK, "/tmp/g-a/egress.sock"));
        with.push((ENV_EGRESS_ADDR, "127.0.0.1:0"));
        let cfg = ProcessConfig::from_lookup(lookup(&with)).unwrap();
        assert!(cfg.egress.unwrap().tcp.unwrap().ip().is_loopback());
        let mut wild = BASE.to_vec();
        wild.push((ENV_EGRESS_SOCK, "/tmp/g-a/egress.sock"));
        wild.push((ENV_EGRESS_ADDR, "0.0.0.0:3128"));
        assert!(ProcessConfig::from_lookup(lookup(&wild)).is_err());
        let mut orphan = BASE.to_vec();
        orphan.push((ENV_EGRESS_ADDR, "127.0.0.1:0"));
        assert!(ProcessConfig::from_lookup(lookup(&orphan)).is_err());
    }

    #[test]
    fn two_guests_get_distinct_endpoints() {
        let mk = |n: &str| {
            let sock = format!("/tmp/g-{n}/egress.sock");
            let uds = format!("/tmp/vam-vsock-{n}.socket");
            let pairs = [
                (ENV_UDS, uds.as_str()),
                (ENV_INSTANCE, "i"),
                (ENV_CID, "7"),
                (ENV_EGRESS_SOCK, sock.as_str()),
            ];
            ProcessConfig::from_lookup(lookup(&pairs)).unwrap()
        };
        let (a, b) = (mk("a"), mk("b"));
        assert_ne!(a.uds_base, b.uds_base);
        assert_ne!(a.egress.unwrap().unix, b.egress.unwrap().unix);
    }

    #[test]
    fn this_build_is_the_process_init() {
        assert_eq!(MODE, "process");
    }
}
