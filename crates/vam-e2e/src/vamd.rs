//! The throwaway vamd: a process the engine starts, watches and stops.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// What the engine needs from a vamd. `VamdProcess` is the real one; tests use a fake.
pub trait Vamd {
    fn start(&mut self) -> Result<(), String>;
    /// Stops it; a no-op when it is not running.
    fn stop(&mut self);
    /// Kills and starts again on the same port and state.
    fn restart(&mut self) -> Result<(), String>;
    fn base_url(&self) -> String;
    /// The node's own certificate, which the client trusts (vamd writes it on its first
    /// start). `None` for a node served over plain HTTP.
    fn ca_path(&self) -> Option<PathBuf> {
        None
    }
    /// True while the process is alive.
    fn alive(&mut self) -> bool;
    fn log_tail(&self, lines: usize) -> String;
    /// The node's state directory (Drive9 store and database), when the runner owns it.
    fn state_dir(&self) -> Option<PathBuf> {
        None
    }
    /// Log files of guests, for `brief.md`.
    fn guest_logs(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

pub struct VamdConfig {
    pub bin: PathBuf,
    pub dir: PathBuf,
    pub scripts_dir: PathBuf,
    pub key: String,
    pub extra_env: Vec<(String, String)>,
    /// Golden-snapshot restore (VM nodes). Process nodes keep it off.
    pub snapshot_restore: bool,
    /// A persistent state dir (the vault lives in its `vamd.db`) instead of `<dir>/state`.
    pub state_dir: Option<PathBuf>,
    /// The vault master key, from the environment. `None`: the throwaway node's test constant.
    pub master_key: Option<String>,
}

/// Where vamd keeps the certificate it makes on first start: `<state dir>/tls/ca.pem`
/// (the state dir is the parent of `VAMD_DB_PATH`, `<dir>/state` here). This is a contract
/// with `vam-orchestrator`'s `tls` module; the runner does not depend on that crate.
pub fn node_ca_path(dir: &Path) -> PathBuf {
    dir.join("state").join("tls").join("ca.pem")
}

impl VamdConfig {
    /// The state dir: the persistent one when given, else `<dir>/state`.
    pub fn state(&self) -> PathBuf {
        self.state_dir
            .clone()
            .unwrap_or_else(|| self.dir.join("state"))
    }
}

pub struct VamdProcess {
    cfg: VamdConfig,
    port: u16,
    child: Option<Child>,
}

fn free_port() -> std::io::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

impl VamdProcess {
    pub fn new(cfg: VamdConfig) -> std::io::Result<VamdProcess> {
        // The telemetry listener is port + 1010 (as the spike did); pick a port whose twin is free too.
        let mut port = free_port()?;
        for _ in 0..20 {
            if port < 60000 && std::net::TcpListener::bind(("127.0.0.1", port + 1010)).is_ok() {
                break;
            }
            port = free_port()?;
        }
        Ok(VamdProcess {
            cfg,
            port,
            child: None,
        })
    }

    pub fn log_path(&self) -> PathBuf {
        self.cfg.dir.join("vamd.log")
    }

    fn spawn(&mut self) -> Result<(), String> {
        let d = &self.cfg.dir;
        let state = self.cfg.state();
        for sub in ["drive9", "spool", "snap"] {
            std::fs::create_dir_all(state.join(sub)).map_err(|e| e.to_string())?;
        }
        std::fs::create_dir_all(d.join("home")).map_err(|e| e.to_string())?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
            .map_err(|e| e.to_string())?;
        let err = log.try_clone().map_err(|e| e.to_string())?;
        let path = std::env::var("PATH").unwrap_or_default();
        let mut cmd = Command::new(&self.cfg.bin);
        cmd.env_clear()
            .env("PATH", path)
            .env("HOME", d.join("home"))
            .env("VAMD_LISTEN", format!("127.0.0.1:{}", self.port))
            .env(
                "VAMD_TELEMETRY_LISTEN",
                format!("127.0.0.1:{}", self.port + 1010),
            )
            .env("VAMD_OTLP_ENABLED", "0")
            .env("VAMD_DB_PATH", state.join("vamd.db"))
            .env("VAMD_SPOOL_DIR", state.join("spool"))
            .env("VAM_DRIVE9_ROOT", state.join("drive9"))
            .env("VAM_SNAPSHOT_ROOT", state.join("snap"))
            .env(
                "VAM_SNAPSHOT_RESTORE",
                if self.cfg.snapshot_restore { "1" } else { "0" },
            )
            .env("VAMD_BOOTSTRAP_API_KEY", &self.cfg.key)
            .env(
                "VAMD_SECRETS_MASTER_KEY",
                self.cfg
                    .master_key
                    .clone()
                    .unwrap_or_else(|| "ab".repeat(32)),
            )
            .env("VAMD_E2E_MOCK_SCRIPTS", &self.cfg.scripts_dir)
            // A default grant lives 90 days; a lead of 90 days less 45 s puts a fresh grant
            // inside the `grant.expiring` window after 45 s (the lifetime still exceeds the lead).
            .env(
                "VAMD_E2E_GRANT_EXPIRING_LEAD_SECS",
                (90 * 86_400 - 45).to_string(),
            )
            // Run events carry a text preview of each reply (`outcomePreview`) only at this tier.
            .env("VAMD_TELEMETRY_CAPTURE", "redacted-full")
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err));
        // The runner's scenarios may seed workspaces from the repo's examples; vamd has no
        // built-in examples path (IT-2.8).
        if let Ok(cwd) = std::env::current_dir() {
            let examples = cwd.join("examples");
            if examples.is_dir() {
                cmd.env("VAMD_EXAMPLES_DIR", examples);
            }
        }
        for (k, v) in &self.cfg.extra_env {
            cmd.env(k, v);
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", self.cfg.bin.display()))?;
        self.child = Some(child);
        Ok(())
    }
}

impl Vamd for VamdProcess {
    fn start(&mut self) -> Result<(), String> {
        self.spawn()
    }

    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = Command::new("kill")
                .args(["-TERM", &c.id().to_string()])
                .status();
            for _ in 0..50 {
                if matches!(c.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn restart(&mut self) -> Result<(), String> {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.spawn()
    }

    fn base_url(&self) -> String {
        // vamd serves TLS by default; the runner uses that default, not `--insecure-dev`.
        format!("https://127.0.0.1:{}", self.port)
    }

    fn ca_path(&self) -> Option<PathBuf> {
        Some(self.cfg.state().join("tls").join("ca.pem"))
    }

    fn state_dir(&self) -> Option<PathBuf> {
        Some(self.cfg.state())
    }

    fn alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn log_tail(&self, lines: usize) -> String {
        tail_of(&self.log_path(), lines)
    }

    fn guest_logs(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
            if depth > 4 {
                return;
            }
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        walk(&p, out, depth + 1);
                    } else if p.file_name().is_some_and(|n| n == "guest.log") {
                        out.push(p);
                    }
                }
            }
        }
        walk(&self.cfg.dir.join("state"), &mut out, 0);
        walk(Path::new("/tmp/vam-process-guest"), &mut out, 0);
        // Firecracker's own stdout and stderr (vam-core `vmm.rs`), for microVM nodes.
        if let Ok(rd) = std::fs::read_dir("/tmp") {
            let mut fc: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                        n.starts_with("vam-fc-")
                            && (n.ends_with(".stdout") || n.ends_with(".stderr"))
                    })
                })
                .collect();
            fc.sort();
            out.extend(fc);
        }
        out
    }
}

impl Drop for VamdProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn tail_of(path: &Path, lines: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(t) => {
            let v: Vec<&str> = t.lines().collect();
            v[v.len().saturating_sub(lines)..].join("\n")
        }
        Err(e) => format!("(no log: {e})"),
    }
}
