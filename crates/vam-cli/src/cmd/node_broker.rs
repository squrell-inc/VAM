//! `vam node init --broker web,schedule,notify`: install the first-party brokers a node asks for
//! and register them with `vamd` through the API (release-artifacts RA-B.4).
//!
//! One template unit `vam-broker@.service`, one argument file per broker under `/etc/vam/`
//! (`broker-<name>.env`, no secret in it: a broker's credentials live in the vault), sockets
//! `0600` under the state directory, started after `vamd`. Registration is `PUT
//! /v1/hosted/brokers` with the manifest the broker binary prints (`<broker> manifest`).
//! Re-running changes nothing that is already right and never touches a broker it was not
//! asked about. `--root DIR` redirects every host path, as for the rest of `node init`.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use super::node_init::{self, Line, Status};
use super::node_install::write_if_changed;
use crate::client::Client;
use crate::config::{ConfigFile, ResolvedConfig};
use vam_spec::v1::{CapabilityManifest, TrustTier};

const STATE_DIR: &str = "/var/lib/vam";
const ETC_DIR: &str = "/etc/vam";
const UNIT_DIR: &str = "/etc/systemd/system";
const DEFAULT_HOST: &str = "https://localhost:8080";
/// `sun_path` holds 108 bytes on Linux; stay clear of the edge.
const MAX_SOCKET_PATH: usize = 100;

/// A first-party broker `vam node init` can install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Broker {
    Web,
    Schedule,
    Notify,
}

impl Broker {
    pub fn name(self) -> &'static str {
        match self {
            Broker::Web => "web",
            Broker::Schedule => "schedule",
            Broker::Notify => "notify",
        }
    }

    /// The broker id `vamd` registers it under.
    pub fn id(self) -> &'static str {
        match self {
            Broker::Web => "vam.web",
            Broker::Schedule => "vam.schedule",
            Broker::Notify => "vam.notify",
        }
    }

    pub fn binary(self) -> &'static str {
        match self {
            Broker::Web => "vam-web-broker",
            Broker::Schedule => "vam-schedule-broker",
            Broker::Notify => "vam-notify-broker",
        }
    }

    /// Schedule and notify keep a database or a log in a directory of their own and are given a
    /// gateway socket by `vamd` once registered.
    fn has_state(self) -> bool {
        !matches!(self, Broker::Web)
    }
}

/// The names as the user typed them, each once, in the order first seen.
pub fn dedupe(brokers: &[Broker]) -> Vec<Broker> {
    let mut out: Vec<Broker> = Vec::new();
    for b in brokers {
        if !out.contains(b) {
            out.push(*b);
        }
    }
    out
}

fn under(root: &Path, abs: &str) -> PathBuf {
    root.join(abs.trim_start_matches('/'))
}

fn brokers_dir() -> String {
    format!("{STATE_DIR}/brokers")
}

fn socket_path(b: Broker) -> String {
    format!("{}/{}.sock", brokers_dir(), b.name())
}

fn state_path(b: Broker) -> String {
    format!("{}/{}", brokers_dir(), b.name())
}

fn args_file(b: Broker) -> String {
    format!("{ETC_DIR}/broker-{}.env", b.name())
}

/// The arguments the broker runs with. `prefix` is `""` for what the unit sees (host paths) and
/// the redirected root for a process started here under `--root`.
fn broker_args(b: Broker, prefix: &str, gateway: Option<&str>) -> Vec<String> {
    let mut a = vec![
        "--socket".to_string(),
        format!("{prefix}{}", socket_path(b)),
    ];
    match b {
        Broker::Web => {
            a.push("--provider".into());
            a.push("brave".into());
        }
        Broker::Schedule | Broker::Notify => {
            a.push("--state-dir".into());
            a.push(format!("{prefix}{}", state_path(b)));
            if let Some(g) = gateway {
                a.push("--gateway-socket".into());
                a.push(g.to_string());
            }
        }
    }
    a
}

/// The argument file. It names paths and flags only: a credential never goes here.
pub(crate) fn args_file_text(b: Broker, gateway: Option<&str>) -> String {
    format!(
        "# vam-broker@{n}: written by `vam node init --broker {n}`. Paths and flags only; the\n\
         # broker's credentials go in the vault (`vam secrets set`), never in this file.\n\
         BROKER_ARGS={args}\n",
        n = b.name(),
        args = broker_args(b, "", gateway).join(" ")
    )
}

/// The `--gateway-socket` an argument file already carries (learned from `vamd` earlier).
pub(crate) fn gateway_in(text: &str) -> Option<String> {
    let line = text.lines().find_map(|l| l.strip_prefix("BROKER_ARGS="))?;
    let mut it = line.split_whitespace();
    while let Some(t) = it.next() {
        if t == "--gateway-socket" {
            return it.next().map(str::to_string);
        }
    }
    None
}

/// The template unit: one instance per broker, the same user as `vamd`, after `vamd`.
pub(crate) fn unit_text(bindir: &str) -> String {
    format!(
        "[Unit]\n\
Description=VAM broker (%i)\n\
After=vamd.service\n\
Wants=vamd.service\n\
\n\
[Service]\n\
Type=simple\n\
User=root\n\
EnvironmentFile={ETC_DIR}/broker-%i.env\n\
ExecStart={bindir}/vam-%i-broker $BROKER_ARGS\n\
Restart=on-failure\n\
RestartSec=3\n\
ProtectSystem=full\n\
ProtectHome=yes\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n"
    )
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
}

/// One check line per requested broker: its binary sits beside `vam` and can run.
pub fn check_lines(brokers: &[Broker], dir: Option<&Path>) -> Vec<Line> {
    brokers
        .iter()
        .map(|b| {
            let check = format!("broker {}", b.binary());
            let fix = format!(
                "copy {} from the node bundle next to vam, or drop {} from --broker",
                b.binary(),
                b.name()
            );
            let Some(dir) = dir else {
                return node_init::problem(
                    &check,
                    Status::Missing,
                    format!("cannot tell where vam is, so {} is not found", b.binary()),
                    fix,
                );
            };
            let path = dir.join(b.binary());
            match std::fs::metadata(&path) {
                Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => {
                    node_init::good(&check, path.display().to_string())
                }
                Ok(m) if m.is_file() => node_init::problem(
                    &check,
                    Status::Wrong,
                    format!("{} is not executable", path.display()),
                    format!("chmod 755 {}", path.display()),
                ),
                _ => node_init::problem(
                    &check,
                    Status::Missing,
                    format!("{} not found beside vam ({})", b.binary(), dir.display()),
                    fix,
                ),
            }
        })
        .collect()
}

/// The lines `--check` and the install print for the requested brokers, next to `vam`.
pub fn requested_check_lines(brokers: &[Broker]) -> Vec<Line> {
    check_lines(brokers, exe_dir().as_deref())
}

/// Where registration goes, and with which key.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub host: String,
    pub key: String,
    pub ca: Option<PathBuf>,
}

/// The key this run registers with: the one just minted (when no other host is named), then `--token`, `VAM_API_KEY`, the
/// signed-in config. No key means nothing is registered. A key minted here belongs to this
/// node, so the node's own certificate is trusted for it unless a CA is already configured.
pub(crate) fn resolve_target(
    minted: Option<&str>,
    host_flag: Option<&str>,
    token_flag: Option<&str>,
    env_host: Option<&str>,
    env_key: Option<&str>,
    file: &ConfigFile,
    own_ca: Option<PathBuf>,
) -> Option<Target> {
    let nonempty = |s: Option<&str>| s.filter(|s| !s.is_empty()).map(str::to_string);
    let explicit_host = nonempty(host_flag)
        .or_else(|| nonempty(env_host))
        .or_else(|| file.host.clone().filter(|s| !s.is_empty()));
    // The minted key opens this node only: it is used when no other host is named.
    let own = explicit_host.is_none();
    let key = nonempty(minted)
        .filter(|_| own)
        .or_else(|| nonempty(token_flag))
        .or_else(|| nonempty(env_key))
        .or_else(|| file.session_token.clone().filter(|s| !s.is_empty()))
        .or_else(|| file.api_key.clone().filter(|s| !s.is_empty()))?;
    let ca = if own { own_ca } else { None };
    Some(Target {
        host: explicit_host.unwrap_or_else(|| DEFAULT_HOST.to_string()),
        key,
        ca,
    })
}

/// The socket accepts a connection: the broker is up.
fn socket_alive(sock: &Path) -> bool {
    UnixStream::connect(sock).is_ok()
}

/// Tries 200 ms apart that a starting broker gets to open its socket (10 s in all).
const SOCKET_TRIES: u32 = 50;

fn wait_for_socket_tries(sock: &Path, tries: u32) -> bool {
    for i in 0..tries {
        if socket_alive(sock) {
            return true;
        }
        if i + 1 < tries {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    false
}

fn wait_for_socket(sock: &Path) -> bool {
    wait_for_socket_tries(sock, SOCKET_TRIES)
}

/// The systemd unit of one broker (an instance of `vam-broker@.service`).
fn unit_name(b: Broker) -> String {
    format!("vam-broker@{}", b.name())
}

/// After systemd (re)started the broker: wait for its socket, so registration finds it up.
fn await_unit_socket(unit: &str, sock: &Path, tries: u32) -> Result<()> {
    if wait_for_socket_tries(sock, tries) {
        return Ok(());
    }
    bail!(
        "{unit} did not open its socket {}; see `journalctl -u {unit}`",
        sock.display()
    )
}

fn pid_path(root: &Path, b: Broker) -> PathBuf {
    under(root, &brokers_dir()).join(format!("{}.pid", b.name()))
}

fn stop_recorded(root: &Path, b: Broker) {
    if let Some(pid) = std::fs::read_to_string(pid_path(root, b))
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok())
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    let _ = std::fs::remove_file(pid_path(root, b));
    let _ = std::fs::remove_file(under(root, &socket_path(b)));
}

/// Start the broker detached from this process (its own process group, no stdin, output to a
/// log file) and record its pid. It has no supervisor: used only under `--no-systemd`.
fn spawn_detached(root: &Path, b: Broker, bin: &Path, gateway: Option<&str>) -> Result<()> {
    let prefix = if root == Path::new("/") {
        String::new()
    } else {
        root.display().to_string().trim_end_matches('/').to_string()
    };
    let dir = under(root, &brokers_dir());
    let log = dir.join(format!("{}.log", b.name()));
    let logf = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("open {}", log.display()))?;
    let child = Command::new(bin)
        .args(broker_args(b, &prefix, gateway))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .stdout(Stdio::from(logf.try_clone()?))
        .stderr(Stdio::from(logf))
        .process_group(0)
        .spawn()
        .with_context(|| format!("start {}", bin.display()))?;
    write_if_changed(
        &pid_path(root, b),
        format!("{}\n", child.id()).as_bytes(),
        0o644,
    )?;
    // Not waited for: it outlives this command.
    drop(child);
    if !wait_for_socket(&under(root, &socket_path(b))) {
        bail!(
            "{} did not open its socket; see {}",
            b.binary(),
            log.display()
        );
    }
    Ok(())
}

fn is_connect_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<reqwest::Error>()
            .is_some_and(|r| r.is_connect() || r.is_timeout())
    })
}

/// What registering one broker did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Registered {
    Unchanged,
    Done,
}

/// The manifest as the proto it is, so omitted defaults and key order compare equal.
fn decode_manifest(v: &Value) -> Option<CapabilityManifest> {
    vam_spec::encoding::json_to_proto("vam.v1.CapabilityManifest", &v.to_string()).ok()
}

/// Whether `vamd` gives this manifest a gateway socket: it may request runs, or it declares a
/// notification channel on a tier that may deliver (core or first-party). The same rule as
/// `wants_gateway` in `vam-orchestrator/src/gateway.rs` with `tier_may_deliver` in
/// `notify_gateway.rs`.
fn needs_gateway(m: &CapabilityManifest) -> bool {
    m.run_request_permission.is_some()
        || (m.notification_channel.is_some()
            && matches!(
                TrustTier::try_from(m.trust_tier),
                Ok(TrustTier::Core | TrustTier::FirstParty)
            ))
}

/// Whether a record from `GET /v1/hosted/brokers` already matches what we would register: the
/// whole manifest (secret bindings, egress, tools with their side effects and destinations, the
/// rest), enabled, at this socket, and holding its gateway socket when it needs one. Anything
/// else is registered again, since `PUT /v1/hosted/brokers` is what refreshes vamd's approval.
pub(crate) fn record_matches(record: &Value, manifest: &Value, sock: &str) -> bool {
    let (Some(have), Some(want)) = (
        decode_manifest(&record["manifest"]),
        decode_manifest(manifest),
    ) else {
        return false;
    };
    let gateway_ok = !needs_gateway(&want)
        || record["gatewaySocket"]
            .as_str()
            .is_some_and(|g| !g.is_empty());
    have == want
        && record["enabled"].as_bool().unwrap_or(false)
        && record["unixPath"].as_str() == Some(sock)
        && gateway_ok
}

/// Register one broker, or find it already registered. Returns what happened and the gateway
/// socket `vamd` reports for it, when it has one.
pub(crate) async fn register_one(
    c: &Client,
    manifest: &Value,
    sock: &str,
) -> Result<(Registered, Option<String>)> {
    let list: Value = serde_json::from_str(&c.get("/v1/hosted/brokers").await?)
        .context("read the broker list from vamd")?;
    if let Some(rec) = list["brokers"].as_array().and_then(|a| {
        a.iter()
            .find(|r| r["manifest"]["brokerId"] == manifest["brokerId"])
    }) {
        if record_matches(rec, manifest, sock) {
            let gw = rec["gatewaySocket"].as_str().map(str::to_string);
            return Ok((Registered::Unchanged, gw));
        }
    }
    let body = json!({ "manifest": manifest, "unixPath": sock }).to_string();
    let out = c.put_raw_json("/v1/hosted/brokers", &body, None).await?;
    let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    let gw = v["broker"]["gatewaySocket"].as_str().map(str::to_string);
    Ok((Registered::Done, gw))
}

fn broker_manifest(bin: &Path) -> Result<Value> {
    let out = Command::new(bin)
        .arg("manifest")
        .output()
        .with_context(|| format!("run {} manifest", bin.display()))?;
    if !out.status.success() {
        bail!("{} manifest failed ({})", bin.display(), out.status);
    }
    serde_json::from_slice(&out.stdout)
        .with_context(|| format!("{} manifest did not print JSON", bin.display()))
}

/// Everything `run_brokers` needs from the install.
pub(crate) struct Plan<'a> {
    pub root: &'a Path,
    pub brokers: &'a [Broker],
    pub no_systemd: bool,
    pub no_start: bool,
    pub vamd_started: bool,
    pub minted_key: Option<&'a str>,
    pub api_host: Option<&'a str>,
    pub api_token: Option<&'a str>,
}

fn unchanged(step: &str, detail: &str) {
    println!("ok (unchanged) {step}: {detail}");
}

fn done(step: &str, detail: &str) {
    println!("done {step}: {detail}");
}

fn systemctl(args: &[&str]) -> bool {
    Command::new("systemctl")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A word the shell reads back as itself: left bare when it holds only safe characters, else
/// single-quoted.
fn sh_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The command that registers `brokers` later. On the real root it runs as root (`/var/lib/vam`
/// is `0700`) with `VAM_API_KEY` after `sudo`, which would drop it from the environment
/// otherwise; `vam` is this binary's absolute path, since `sudo` resets `PATH`. It carries
/// `--root`, `--no-systemd` and `--host` as given, never `--token` or a key: `<admin key>` stays
/// a placeholder.
pub(crate) fn register_command(
    vam: &Path,
    root: &Path,
    no_systemd: bool,
    host: Option<&str>,
    brokers: &[Broker],
) -> String {
    let real_root = root == Path::new("/");
    let mut cmd = String::new();
    if real_root {
        cmd.push_str("sudo ");
    }
    cmd.push_str("VAM_API_KEY=<admin key> ");
    cmd.push_str(&sh_quote(&vam.display().to_string()));
    cmd.push_str(" node init");
    if !real_root {
        cmd.push_str(" --root ");
        cmd.push_str(&sh_quote(&root.display().to_string()));
    }
    if no_systemd {
        cmd.push_str(" --no-systemd");
    }
    if let Some(h) = host.filter(|h| !h.is_empty()) {
        cmd.push_str(" --host ");
        cmd.push_str(&sh_quote(h));
    }
    let names: Vec<&str> = brokers.iter().map(|b| b.name()).collect();
    cmd.push_str(" --broker ");
    cmd.push_str(&names.join(","));
    cmd
}

fn print_register_command(p: &Plan<'_>, b: Broker, why: &str) {
    println!(
        "broker {}: nothing was sent to vamd ({why}). To register it, use an API key that has brokers:admin and run:",
        b.id()
    );
    let vam = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("vam"));
    println!(
        "  {}",
        register_command(&vam, p.root, p.no_systemd, p.api_host, &[b])
    );
}

async fn wait_ready(c: &Client) {
    for _ in 0..30 {
        if c.get("/v1/node").await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Install and register the requested brokers. Nothing is removed and no other broker is
/// touched. Exit stays 0 when the only gap is a key or a `vamd` that is not up yet.
pub(crate) async fn run_brokers(p: Plan<'_>) -> Result<()> {
    if p.brokers.is_empty() {
        return Ok(());
    }
    let dir = exe_dir().ok_or_else(|| anyhow!("cannot tell where vam is"))?;
    let real_root = p.root == Path::new("/");
    let bdir = under(p.root, &brokers_dir());
    std::fs::create_dir_all(&bdir)?;
    std::fs::set_permissions(&bdir, std::fs::Permissions::from_mode(0o700))?;

    let mut unit_changed = false;
    if !p.no_systemd {
        let path = under(p.root, &format!("{UNIT_DIR}/vam-broker@.service"));
        unit_changed = write_if_changed(
            &path,
            unit_text(&dir.display().to_string()).as_bytes(),
            0o644,
        )?;
        if unit_changed {
            done("broker-unit", &format!("wrote {}", path.display()));
        } else {
            unchanged("broker-unit", &path.display().to_string());
        }
    }

    // Files and processes first, so the socket is there when vamd dials it.
    let mut args_changed = Vec::new();
    for b in p.brokers {
        let sock = under(p.root, &socket_path(*b));
        if sock.as_os_str().len() > MAX_SOCKET_PATH {
            bail!(
                "the socket path {} is too long for a Unix socket; use a shorter --root",
                sock.display()
            );
        }
        if b.has_state() {
            let sd = under(p.root, &state_path(*b));
            std::fs::create_dir_all(&sd)?;
            std::fs::set_permissions(&sd, std::fs::Permissions::from_mode(0o700))?;
        }
        let file = under(p.root, &args_file(*b));
        let gateway = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| gateway_in(&t));
        let text = args_file_text(*b, gateway.as_deref());
        let changed = write_if_changed(&file, text.as_bytes(), 0o644)?;
        args_changed.push(changed);
        let step = format!("broker-args {}", b.id());
        if changed {
            done(&step, &format!("wrote {}", file.display()));
        } else {
            unchanged(&step, &file.display().to_string());
        }
        start_broker(
            &p,
            *b,
            &dir,
            gateway.as_deref(),
            changed,
            unit_changed,
            real_root,
        )?;
    }

    // Registration.
    let file = ConfigFile::load().unwrap_or_default();
    let own_ca = {
        let ca = under(p.root, STATE_DIR).join("tls/ca.pem");
        ca.is_file().then_some(ca)
    };
    let target = resolve_target(
        p.minted_key,
        p.api_host,
        p.api_token,
        std::env::var("VAM_HOST").ok().as_deref(),
        std::env::var("VAM_API_KEY").ok().as_deref(),
        &file,
        own_ca,
    );
    let client = match &target {
        Some(t) if !p.no_start || p.no_systemd => Some(Client::new_with_ca(
            &ResolvedConfig {
                host: t.host.clone(),
                api_key: t.key.clone(),
            },
            t.ca.as_deref(),
        )?),
        _ => None,
    };
    let Some(client) = client else {
        let why = if target.is_none() {
            "no API key"
        } else {
            "--no-start: nothing is running yet"
        };
        for b in p.brokers {
            print_register_command(&p, *b, why);
        }
        return Ok(());
    };
    if p.vamd_started {
        wait_ready(&client).await;
    }
    for b in p.brokers {
        let sock = under(p.root, &socket_path(*b)).display().to_string();
        let manifest = broker_manifest(&dir.join(b.binary()))?;
        match register_one(&client, &manifest, &sock).await {
            Ok((outcome, gateway)) => {
                let step = format!("broker {}", b.id());
                match outcome {
                    Registered::Unchanged => unchanged(&step, "registered"),
                    Registered::Done => done(&step, "registered"),
                }
                if let Some(g) = gateway.filter(|_| b.has_state()) {
                    let file = under(p.root, &args_file(*b));
                    let known = std::fs::read_to_string(&file)
                        .ok()
                        .and_then(|t| gateway_in(&t));
                    if known.as_deref() != Some(g.as_str()) {
                        write_if_changed(&file, args_file_text(*b, Some(&g)).as_bytes(), 0o644)?;
                        done(
                            &format!("broker-args {}", b.id()),
                            "added the gateway socket vamd reported",
                        );
                        restart_broker(&p, *b, &dir, &g, real_root)?;
                    }
                }
            }
            Err(e) if is_connect_error(&e) => {
                print_register_command(&p, *b, "vamd is not reachable");
            }
            Err(e) => return Err(e.context(format!("register {}", b.id()))),
        }
    }
    Ok(())
}

/// Make sure the broker runs: under systemd enable it (and restart it when its unit or
/// arguments changed); otherwise start it detached when nothing answers on its socket.
fn start_broker(
    p: &Plan<'_>,
    b: Broker,
    dir: &Path,
    gateway: Option<&str>,
    args_changed: bool,
    unit_changed: bool,
    real_root: bool,
) -> Result<()> {
    let sock = under(p.root, &socket_path(b));
    if p.no_systemd {
        if socket_alive(&sock) && !args_changed {
            unchanged(
                &format!("broker-process {}", b.id()),
                &format!("running (pid file {})", pid_path(p.root, b).display()),
            );
            return Ok(());
        }
        if socket_alive(&sock) {
            stop_recorded(p.root, b);
        }
        spawn_detached(p.root, b, &dir.join(b.binary()), gateway)?;
        let log = under(p.root, &brokers_dir()).join(format!("{}.log", b.name()));
        done(
            &format!("broker-process {}", b.id()),
            &format!(
                "started without a supervisor (--no-systemd); log {}, pid file {}",
                log.display(),
                pid_path(p.root, b).display()
            ),
        );
        return Ok(());
    }
    if p.no_start || !real_root {
        return Ok(());
    }
    let unit = unit_name(b);
    let _ = systemctl(&["daemon-reload"]);
    let active = systemctl(&["is-active", "--quiet", &unit]);
    if !active {
        if !systemctl(&["enable", "--now", &unit]) {
            bail!("systemctl enable --now {unit} failed; see `journalctl -u {unit}`");
        }
        await_unit_socket(&unit, &sock, SOCKET_TRIES)?;
        done(&format!("broker-service {}", b.id()), "enabled and started");
    } else if args_changed || unit_changed {
        systemctl_restart(&unit)?;
        await_unit_socket(&unit, &sock, SOCKET_TRIES)?;
        done(&format!("broker-service {}", b.id()), "restarted");
    } else {
        unchanged(&format!("broker-service {}", b.id()), "enabled and running");
    }
    Ok(())
}

fn restart_broker(
    p: &Plan<'_>,
    b: Broker,
    dir: &Path,
    gateway: &str,
    real_root: bool,
) -> Result<()> {
    if p.no_systemd {
        stop_recorded(p.root, b);
        return spawn_detached(p.root, b, &dir.join(b.binary()), Some(gateway));
    }
    if real_root && !p.no_start {
        let unit = unit_name(b);
        systemctl_restart(&unit)?;
        await_unit_socket(&unit, &under(p.root, &socket_path(b)), SOCKET_TRIES)?;
    }
    Ok(())
}

fn systemctl_restart(unit: &str) -> Result<()> {
    if !systemctl(&["restart", unit]) {
        bail!("systemctl restart {unit} failed; see `journalctl -u {unit}`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[arg(long = "broker", value_delimiter = ',', value_enum)]
        brokers: Vec<Broker>,
    }

    fn parse(a: &[&str]) -> Result<Vec<Broker>, clap::Error> {
        let mut v = vec!["t"];
        v.extend_from_slice(a);
        T::try_parse_from(v).map(|t| dedupe(&t.brokers))
    }

    #[test]
    fn broker_flag_takes_a_list_repeats_dedupes_and_defaults_to_none() {
        assert_eq!(parse(&[]).unwrap(), vec![]);
        assert_eq!(
            parse(&["--broker", "web,schedule"]).unwrap(),
            vec![Broker::Web, Broker::Schedule]
        );
        assert_eq!(
            parse(&["--broker", "notify", "--broker", "web,notify"]).unwrap(),
            vec![Broker::Notify, Broker::Web]
        );
        let e = parse(&["--broker", "web,files"]).unwrap_err().to_string();
        assert!(e.contains("files") && e.contains("schedule"), "{e}");
        assert!(parse(&["--broker", "all"]).is_err());
    }

    #[test]
    fn the_unit_is_a_template_after_vamd_with_no_secret_and_no_weak_switch() {
        let u = unit_text("/opt/vam/bin");
        assert!(u.contains("After=vamd.service"));
        assert!(u.contains("EnvironmentFile=/etc/vam/broker-%i.env"));
        assert!(u.contains("ExecStart=/opt/vam/bin/vam-%i-broker $BROKER_ARGS"));
        for bad in ["--insecure", "--no-jailer", "KEY", "TOKEN", "SECRET"] {
            assert!(!u.contains(bad), "{bad}");
        }
    }

    #[test]
    fn argument_files_hold_paths_and_flags_and_keep_a_learned_gateway() {
        let web = args_file_text(Broker::Web, None);
        assert!(web.contains("BROKER_ARGS=--socket /var/lib/vam/brokers/web.sock --provider brave"));
        assert!(!web.to_lowercase().contains("key="));
        let s = args_file_text(Broker::Schedule, Some("/run/vam/gw.sock"));
        assert!(s.contains("--state-dir /var/lib/vam/brokers/schedule"));
        assert_eq!(gateway_in(&s).as_deref(), Some("/run/vam/gw.sock"));
        assert_eq!(gateway_in(&args_file_text(Broker::Notify, None)), None);
        // The web broker never gets a gateway.
        assert_eq!(gateway_in(&args_file_text(Broker::Web, Some("/x"))), None);
    }

    #[test]
    fn a_requested_broker_binary_is_checked_beside_vam() {
        let dir = tempfile::tempdir().unwrap();
        let lines = check_lines(&[Broker::Web, Broker::Notify], Some(dir.path()));
        assert_eq!(lines[0].check, "broker vam-web-broker");
        assert_eq!(lines[0].status, Status::Missing);
        assert!(lines[0]
            .detail
            .contains("vam-web-broker not found beside vam"));
        assert!(lines[0].fix.contains("drop web from --broker"));
        let bin = dir.path().join("vam-web-broker");
        std::fs::write(&bin, "x").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            check_lines(&[Broker::Web], Some(dir.path()))[0].status,
            Status::Wrong
        );
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            check_lines(&[Broker::Web], Some(dir.path()))[0].status,
            Status::Ok
        );
        assert!(check_lines(&[], Some(dir.path())).is_empty());
    }

    #[test]
    fn the_key_comes_from_the_mint_then_the_flag_then_the_env_then_the_config() {
        let file = ConfigFile {
            host: Some("https://cfg:1".into()),
            api_key: Some("cfgkey".into()),
            ..Default::default()
        };
        let ca = Some(PathBuf::from("/s/ca.pem"));
        let t = resolve_target(
            Some("mint"),
            None,
            Some("flag"),
            None,
            Some("env"),
            &file,
            ca.clone(),
        )
        .unwrap();
        assert_eq!(
            (t.key.as_str(), t.host.as_str(), t.ca.is_none()),
            ("flag", "https://cfg:1", true),
            "a minted key opens this node only, not a host the user named"
        );
        let no_host = ConfigFile {
            api_key: Some("cfgkey".into()),
            ..Default::default()
        };
        let t = resolve_target(
            Some("mint"),
            None,
            Some("flag"),
            None,
            None,
            &no_host,
            ca.clone(),
        )
        .unwrap();
        assert_eq!((t.key.as_str(), t.host.as_str()), ("mint", DEFAULT_HOST));
        assert_eq!(t.ca, ca);
        let t = resolve_target(
            None,
            Some("https://h:2"),
            Some("flag"),
            None,
            Some("env"),
            &file,
            ca.clone(),
        )
        .unwrap();
        assert_eq!(
            (t.key.as_str(), t.host.as_str(), t.ca.is_none()),
            ("flag", "https://h:2", true)
        );
        assert_eq!(
            resolve_target(None, None, None, None, Some("env"), &file, None)
                .unwrap()
                .key,
            "env"
        );
        assert_eq!(
            resolve_target(None, None, None, None, None, &file, None)
                .unwrap()
                .key,
            "cfgkey"
        );
        let none = ConfigFile::default();
        assert!(resolve_target(None, None, None, None, None, &none, None).is_none());
        let t = resolve_target(Some("m"), None, None, None, None, &none, ca.clone()).unwrap();
        assert_eq!((t.host.as_str(), t.ca), (DEFAULT_HOST, ca));
    }

    /// The web manifest as `vam-web-broker manifest` prints it.
    fn web_manifest() -> Value {
        json!({
            "brokerId": "vam.web",
            "trustTier": "TRUST_TIER_FIRST_PARTY",
            "secretBindings": [
                {"logicalName": "brave-api-key", "vaultKey": "web/brave-api-key", "required": true}
            ],
            "egressDomains": ["api.search.brave.com"],
            "tools": [
                {"toolName": "web_search", "sideEffect": "SIDE_EFFECT_CLASS_READ_ONLY",
                 "requiredSecrets": ["brave-api-key"], "destinationPolicy": "TOOL_DESTINATION_ALLOWLIST"},
                {"toolName": "web_fetch", "sideEffect": "SIDE_EFFECT_CLASS_READ_ONLY",
                 "requiredSecrets": [], "destinationPolicy": "TOOL_DESTINATION_PUBLIC_WEB"}
            ],
            "labels": {"vam.capability": "web"}
        })
    }

    /// The same manifest as vamd lists it: defaults left out, snake_case keys, keys in another
    /// order.
    fn web_record() -> Value {
        let text = r#"{"enabled": true, "unixPath": "/s", "tools": [], "manifest": {
            "labels": {"vam.capability": "web"},
            "tools": [
                {"tool_name": "web_search", "required_secrets": ["brave-api-key"],
                 "side_effect": "SIDE_EFFECT_CLASS_READ_ONLY"},
                {"destinationPolicy": "TOOL_DESTINATION_PUBLIC_WEB",
                 "sideEffect": "SIDE_EFFECT_CLASS_READ_ONLY", "toolName": "web_fetch"}
            ],
            "egressDomains": ["api.search.brave.com"],
            "secretBindings": [
                {"required": true, "vaultKey": "web/brave-api-key", "logicalName": "brave-api-key"}
            ],
            "trustTier": "TRUST_TIER_FIRST_PARTY",
            "brokerId": "vam.web"
        }}"#;
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn an_equal_manifest_with_defaults_dropped_and_keys_reordered_matches() {
        let (manifest, rec) = (web_manifest(), web_record());
        assert!(record_matches(&rec, &manifest, "/s"));
        assert!(!record_matches(&rec, &manifest, "/other"));
        let mut off = rec.clone();
        off["enabled"] = json!(false);
        assert!(!record_matches(&off, &manifest, "/s"));
        let mut no_path = rec.clone();
        no_path.as_object_mut().unwrap().remove("unixPath");
        assert!(!record_matches(&no_path, &manifest, "/s"));
        let mut garbled = rec.clone();
        garbled["manifest"] = json!({"brokerId": 7});
        assert!(!record_matches(&garbled, &manifest, "/s"));
    }

    #[test]
    fn a_manifest_change_without_a_tool_rename_does_not_match() {
        type Change = (&'static str, fn(&mut Value));
        let rec = web_record();
        let changes: [Change; 7] = [
            ("secretBindings logical name", |m| {
                m["secretBindings"][0]["logicalName"] = json!("brave-key")
            }),
            ("secretBindings required", |m| {
                m["secretBindings"][0]["required"] = json!(false)
            }),
            ("vaultKey", |m| {
                m["secretBindings"][0]["vaultKey"] = json!("web/brave-api-key-2")
            }),
            ("egressDomains", |m| {
                m["egressDomains"] = json!(["api.search.brave.com", "example.com"])
            }),
            ("sideEffect", |m| {
                m["tools"][1]["sideEffect"] = json!("SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL")
            }),
            ("destinationPolicy", |m| {
                m["tools"][1]["destinationPolicy"] = json!("TOOL_DESTINATION_ALLOWLIST")
            }),
            ("requiredSecrets", |m| {
                m["tools"][1]["requiredSecrets"] = json!(["brave-api-key"])
            }),
        ];
        for (what, change) in changes {
            let mut manifest = web_manifest();
            change(&mut manifest);
            assert!(decode_manifest(&manifest).is_some(), "{what}: decodes");
            assert!(!record_matches(&rec, &manifest, "/s"), "{what}");
        }
    }

    #[test]
    fn a_record_that_needs_a_gateway_and_has_none_does_not_match() {
        let manifest = json!({
            "brokerId": "vam.schedule",
            "trustTier": "TRUST_TIER_FIRST_PARTY",
            "tools": [{"toolName": "schedule_create"}],
            "runRequestPermission": {"grantTools": ["schedule_create"], "surface": "schedule"}
        });
        let mut rec = json!({"manifest": manifest.clone(), "unixPath": "/s", "enabled": true});
        assert!(!record_matches(&rec, &manifest, "/s"), "no gatewaySocket");
        rec["gatewaySocket"] = json!("");
        assert!(
            !record_matches(&rec, &manifest, "/s"),
            "empty gatewaySocket"
        );
        rec["gatewaySocket"] = json!("/run/vam/gw/vam.schedule.sock");
        assert!(record_matches(&rec, &manifest, "/s"));

        // A channel broker needs one on a tier that may deliver, and not on any other.
        let channel = json!({
            "brokerId": "vam.notify",
            "trustTier": "TRUST_TIER_FIRST_PARTY",
            "notificationChannel": {"channel": "slack", "surface": "slack"}
        });
        let rec = json!({"manifest": channel.clone(), "unixPath": "/s", "enabled": true});
        assert!(!record_matches(&rec, &channel, "/s"));
        let mut third = channel.clone();
        third["trustTier"] = json!("TRUST_TIER_THIRD_PARTY");
        let rec = json!({"manifest": third.clone(), "unixPath": "/s", "enabled": true});
        assert!(record_matches(&rec, &third, "/s"));
        // The web broker never needs one.
        assert!(record_matches(&web_record(), &web_manifest(), "/s"));
    }

    #[test]
    fn the_register_command_runs_this_vam_as_root_with_the_key_after_sudo() {
        let vam = Path::new("/opt/vam/bin/vam");
        assert_eq!(
            register_command(vam, Path::new("/"), false, None, &[Broker::Web]),
            "sudo VAM_API_KEY=<admin key> /opt/vam/bin/vam node init --broker web"
        );
        assert_eq!(
            register_command(
                vam,
                Path::new("/"),
                true,
                Some("https://node.example:8080"),
                &[Broker::Web, Broker::Schedule]
            ),
            "sudo VAM_API_KEY=<admin key> /opt/vam/bin/vam node init --no-systemd \
             --host https://node.example:8080 --broker web,schedule"
        );
        // A redirected root is the caller's own tree: no sudo, and every path quoted.
        assert_eq!(
            register_command(
                Path::new("/home/a b/vam's/vam"),
                Path::new("/tmp/my root"),
                true,
                None,
                &[Broker::Notify]
            ),
            r#"VAM_API_KEY=<admin key> '/home/a b/vam'\''s/vam' node init --root '/tmp/my root' --no-systemd --broker notify"#
        );
        assert_eq!(
            register_command(vam, Path::new("/srv/n"), false, Some(""), &[Broker::Web]),
            "VAM_API_KEY=<admin key> /opt/vam/bin/vam node init --root /srv/n --broker web"
        );
    }

    #[test]
    fn a_broker_unit_that_never_opens_its_socket_points_to_its_journal() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("web.sock");
        let unit = unit_name(Broker::Web);
        assert_eq!(unit, "vam-broker@web");
        let e = await_unit_socket(&unit, &sock, 1).unwrap_err().to_string();
        assert!(e.contains("journalctl -u vam-broker@web"), "{e}");
        assert!(e.contains(&sock.display().to_string()), "{e}");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(await_unit_socket(&unit, &sock, 1).is_ok());
    }

    #[tokio::test]
    async fn registering_puts_once_and_then_finds_it_registered() {
        use httpmock::prelude::*;
        let server = MockServer::start();
        let manifest = json!({"brokerId": "vam.web", "tools": [{"toolName": "web_search"}]});
        let mut empty = server.mock(|w, t| {
            w.method(GET).path("/v1/hosted/brokers");
            t.status(200).body(r#"{"brokers":[]}"#);
        });
        let put = server.mock(|w, t| {
            w.method(PUT)
                .path("/v1/hosted/brokers")
                .header_exists("idempotency-key")
                .body_includes("\"unixPath\":\"/s\"");
            t.status(200).body(r#"{"broker":{}}"#);
        });
        let c = Client::new_with_ca(
            &ResolvedConfig {
                host: server.base_url(),
                api_key: "k".into(),
            },
            None,
        )
        .unwrap();
        let (r, _) = register_one(&c, &manifest, "/s").await.unwrap();
        assert_eq!(r, Registered::Done);
        put.assert_calls(1);
        empty.delete();
        server.mock(|w, t| {
            w.method(GET).path("/v1/hosted/brokers");
            t.status(200).body(
                r#"{"brokers":[{"manifest":{"brokerId":"vam.web","tools":[{"toolName":"web_search"}]},
                "unixPath":"/s","enabled":true}]}"#,
            );
        });
        let (r, _) = register_one(&c, &manifest, "/s").await.unwrap();
        assert_eq!(r, Registered::Unchanged);
        put.assert_calls(1);
    }
}
