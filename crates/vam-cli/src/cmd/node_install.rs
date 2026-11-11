//! `vam node init` (without `--check`): install a node, safe to run twice (workstream
//! `machine-base`, MB-1.9).
//!
//! Every step compares before it writes and prints `ok (unchanged)` when it did nothing, so a
//! second run changes no file. `--root DIR` redirects every host path (state dir, assets,
//! `/etc/vam`, the unit directory) under DIR; the command then never needs real root or the
//! real `/dev`. Nothing here weakens the jailer or TLS: the unit has no `--no-jailer` and no
//! `--insecure-dev`. The first admin key is relayed from the `vamd --init` stdout line to our
//! stdout and goes nowhere else: no file, no log, no error text.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use super::node_init::{self, Line};
use crate::output::{self, ExitCode};

const STATE_DIR: &str = "/var/lib/vam";
const ASSETS_DIR: &str = "/var/lib/vam/assets";
const ENV_FILE: &str = "/etc/vam/vamd.env";
const UNIT_FILE: &str = "/etc/systemd/system/vamd.service";
const DEFAULT_PORT: u16 = 8080;

/// What the caller asked for.
pub struct Options {
    pub root: PathBuf,
    pub vamd: Option<PathBuf>,
    pub no_systemd: bool,
    pub no_start: bool,
    pub base: Option<String>,
    pub base_sha256: Option<String>,
    pub host_name: Option<String>,
    pub checkout: Option<PathBuf>,
}

fn under(root: &Path, abs: &str) -> PathBuf {
    root.join(abs.trim_start_matches('/'))
}

fn unchanged(step: &str, detail: &str) {
    println!("ok (unchanged) {step}: {detail}");
}

fn done(step: &str, detail: &str) {
    println!("done {step}: {detail}");
}

fn sha256_bytes(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn sha256_path(p: &Path) -> Result<String> {
    let mut f = std::fs::File::open(p).with_context(|| format!("read {}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|x| format!("{x:02x}")).collect())
}

/// Write `body` to `path` with `mode` only when the content or the mode differs. Returns
/// whether anything was written. Written to a sibling temp file and renamed, so a crash never
/// leaves a half file.
pub(crate) fn write_if_changed(path: &Path, body: &[u8], mode: u32) -> Result<bool> {
    let same_body = std::fs::read(path).map(|b| b == body).unwrap_or(false);
    let same_mode = std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777 == mode)
        .unwrap_or(false);
    if same_body && same_mode {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    if same_body {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        return Ok(true);
    }
    let tmp = path.with_extension("part");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(body)?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

/// The env file. Paths are the host's own (what the unit will see), never redirected.
pub(crate) fn env_file_text(tls_sans: Option<&str>) -> String {
    let mut s = String::from(
        "# Written by `vam node init`. Paths and settings only: no secrets belong here.\n",
    );
    s.push_str(&format!(
        "VAMD_FIRECRACKER_BIN={ASSETS_DIR}/firecracker\n\
         VAMD_JAILER_BIN={ASSETS_DIR}/jailer\n\
         VAM_KERNEL_PATH={ASSETS_DIR}/vmlinux\n\
         VAM_ROOTFS_PATH={ASSETS_DIR}/rootfs.ext4\n\
         VAMD_DB_PATH={STATE_DIR}/state.db\n"
    ));
    if let Some(sans) = tls_sans.filter(|s| !s.is_empty()) {
        s.push_str(&format!("VAMD_TLS_SANS={sans}\n"));
    }
    s
}

/// The systemd unit. vamd runs as root because the jailer needs it. The stronger sandbox
/// options (`PrivateDevices`, `PrivateTmp`, `ProtectKernel*`, `RestrictNamespaces`) are absent
/// on purpose: the jailer mounts and enters namespaces, and vamd and the jailer use /tmp sockets.
pub(crate) fn unit_text(vamd: &str) -> String {
    format!(
        "[Unit]\n\
Description=VAM node (vamd)\n\
After=network-online.target\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
User=root\n\
EnvironmentFile={ENV_FILE}\n\
ExecStart={vamd}\n\
Restart=on-failure\n\
RestartSec=3\n\
LimitNOFILE=65536\n\
# Deliberately not set: PrivateDevices, PrivateTmp, ProtectKernel*, RestrictNamespaces.\n\
# The jailer needs mounts, namespaces, /dev/kvm and /tmp sockets. Never add the jailer-off\n\
# or plain-HTTP switches here.\n\
ProtectSystem=full\n\
ProtectHome=yes\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n"
    )
}

/// The value of `KEY=` in an env file.
fn env_value(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(str::to_string))
}

fn find_vamd(opt: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = opt {
        return Ok(p.to_path_buf());
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("vamd")))
        .filter(|p| p.is_file())
        .ok_or_else(|| anyhow!("no vamd binary found beside vam; install it or pass --vamd PATH"))
}

/// A checkout holds `scripts/build-rootfs.sh`: the explicit one, the cwd, or above the binary.
fn find_checkout(opt: Option<&Path>) -> Option<PathBuf> {
    let ok = |p: &Path| p.join("scripts/build-rootfs.sh").is_file();
    if let Some(p) = opt {
        return ok(p).then(|| p.to_path_buf());
    }
    if let Ok(cwd) = std::env::current_dir() {
        if ok(&cwd) {
            return Some(cwd);
        }
    }
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .skip(1)
        .find(|p| ok(p))
        .map(Path::to_path_buf)
}

/// A hash of what goes into the image: the build script, the base files, the guest runtime
/// source and the lockfile. Recorded in `rootfs.stamp` after a build here.
fn build_stamp(checkout: &Path) -> String {
    fn walk(p: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
    }
    let mut files = vec![
        checkout.join("scripts/build-rootfs.sh"),
        checkout.join("Cargo.lock"),
    ];
    walk(&checkout.join("scripts/base"), &mut files);
    walk(&checkout.join("crates/vam-runtime"), &mut files);
    files.retain(|p| !p.components().any(|c| c.as_os_str() == "target"));
    files.sort();
    let mut h = Sha256::new();
    for f in files {
        if let Ok(b) = std::fs::read(&f) {
            h.update(
                f.strip_prefix(checkout)
                    .unwrap_or(&f)
                    .to_string_lossy()
                    .as_bytes(),
            );
            h.update(&b);
        }
    }
    h.finalize().iter().map(|x| format!("{x:02x}")).collect()
}

async fn download(url: &str) -> Result<Vec<u8>> {
    let resp = reqwest::get(url)
        .await
        .with_context(|| format!("download {url}"))?
        .error_for_status()
        .with_context(|| format!("download {url}"))?;
    Ok(resp.bytes().await?.to_vec())
}

/// Put a verified file in place, only when its sha256 is the pinned one. Returns whether it
/// was written.
fn place(dest: &Path, bytes: &[u8], want_sha: &str, mode: u32) -> Result<bool> {
    let got = sha256_bytes(bytes);
    if got != want_sha {
        bail!(
            "sha256 mismatch for {}: got {got}, pinned {want_sha}; nothing was written",
            dest.display()
        );
    }
    write_if_changed(dest, bytes, mode)
}

async fn step_assets(root: &Path) -> Result<()> {
    let assets = under(root, ASSETS_DIR);
    std::fs::create_dir_all(&assets)?;
    let lines = node_init::run_asset_checks(root);
    let bad = |n: &str| {
        lines
            .iter()
            .any(|l| l.check == n && l.status.needs_attention())
    };
    let mut pins: Vec<(String, String)> = std::fs::read_to_string(assets.join("assets.pins"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let mut changed = false;
    if bad("firecracker") || bad("jailer") {
        let ver = node_init::pin_value("FC_VERSION");
        let url = format!(
            "https://github.com/firecracker-microvm/firecracker/releases/download/{ver}/firecracker-{ver}-x86_64.tgz"
        );
        let tgz = download(&url).await?;
        let got = sha256_bytes(&tgz);
        if got != node_init::pin_value("FC_SHA256") {
            bail!("sha256 mismatch for the Firecracker release: got {got}; nothing was written");
        }
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&tgz[..]));
        let (mut fc, mut jl) = (None, None);
        for e in ar.entries()? {
            let mut e = e?;
            let name = e
                .path()?
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let mut b = Vec::new();
            if name == format!("firecracker-{ver}-x86_64") {
                e.read_to_end(&mut b)?;
                fc = Some(b);
            } else if name == format!("jailer-{ver}-x86_64") {
                e.read_to_end(&mut b)?;
                jl = Some(b);
            }
        }
        let (fc, jl) = (
            fc.ok_or_else(|| anyhow!("the release has no firecracker binary"))?,
            jl.ok_or_else(|| anyhow!("the release has no jailer binary"))?,
        );
        for (name, b) in [("firecracker", fc), ("jailer", jl)] {
            write_if_changed(&assets.join(name), &b, 0o755)?;
            pins.retain(|(n, _)| n != name);
            pins.push((name.to_string(), sha256_bytes(&b)));
        }
        changed = true;
    }
    if bad("kernel") {
        let url = node_init::pin_value("KERNEL_URL")
            .replace("${KERNEL_NAME}", &node_init::pin_value("KERNEL_NAME"));
        let k = download(&url).await?;
        let want = node_init::pin_value("KERNEL_SHA256");
        place(&assets.join("vmlinux"), &k, &want, 0o644)?;
        pins.retain(|(n, _)| n != "kernel");
        pins.push(("kernel".into(), want));
        changed = true;
    }
    if changed {
        pins.sort();
        let text: String = pins.iter().map(|(n, v)| format!("{n}={v}\n")).collect();
        write_if_changed(&assets.join("assets.pins"), text.as_bytes(), 0o644)?;
        done(
            "assets",
            "firecracker, jailer and the guest kernel are in place (sha256-verified)",
        );
    } else {
        unchanged(
            "assets",
            "firecracker, jailer and the guest kernel are in place",
        );
    }
    Ok(())
}

async fn step_base(root: &Path, o: &Options) -> Result<()> {
    let assets = under(root, ASSETS_DIR);
    let image = assets.join("rootfs.ext4");
    let manifest = assets.join("rootfs.base.json");
    let stamp_file = assets.join("rootfs.stamp");
    let valid = node_init::base_is_ok(root);
    if let Some(src) = &o.base {
        let want = o
            .base_sha256
            .as_deref()
            .ok_or_else(|| anyhow!("--base needs --base-sha256"))?
            .to_ascii_lowercase();
        if valid && sha256_path(&image).ok().as_deref() == Some(want.as_str()) {
            unchanged("base", "the image is in place and matches --base-sha256");
            return Ok(());
        }
        let fetch = |s: String| async move {
            if s.starts_with("http://") || s.starts_with("https://") {
                download(&s).await
            } else {
                std::fs::read(&s).with_context(|| format!("read {s}"))
            }
        };
        let bytes = fetch(src.clone()).await?;
        if sha256_bytes(&bytes) != want {
            bail!("sha256 mismatch for --base {src}; nothing was written");
        }
        // The manifest travels beside the base: `<src without .ext4>.base.json`.
        let man = fetch(format!("{}.base.json", src.trim_end_matches(".ext4"))).await?;
        write_if_changed(&image, &bytes, 0o644)?;
        write_if_changed(&manifest, &man, 0o644)?;
        done("base", "placed the given base image and its manifest");
        return Ok(());
    }
    let checkout = find_checkout(o.checkout.as_deref());
    let want_stamp = checkout.as_deref().map(build_stamp);
    let have_stamp = std::fs::read_to_string(&stamp_file).ok();
    let stale = matches!((&want_stamp, &have_stamp), (Some(w), Some(h)) if w.trim() != h.trim());
    if image.is_file() && manifest.is_file() && valid && !stale {
        unchanged("base", "the base image matches its manifest");
        return Ok(());
    }
    let Some(checkout) = checkout else {
        bail!("the base image is missing and no checkout was found to build it from: run from the repository, pass --checkout DIR, or pass --base PATH|URL --base-sha256 SHA");
    };
    if root != Path::new("/") {
        bail!("refusing to build the base image under --root: pass --base PATH --base-sha256 SHA");
    }
    println!("building the base image (this takes a while) ...");
    let status = Command::new("bash")
        .arg("scripts/build-rootfs.sh")
        .current_dir(&checkout)
        .env("VAM_ROOTFS_OUT", &image)
        .status()
        .context("run scripts/build-rootfs.sh (needs docker, e2fsprogs and musl-gcc)")?;
    if !status.success() {
        bail!("scripts/build-rootfs.sh failed ({status})");
    }
    if let Some(w) = want_stamp {
        write_if_changed(&stamp_file, w.as_bytes(), 0o644)?;
    }
    done("base", "built the base image from the checkout");
    Ok(())
}

/// Run `vamd --init` against the redirected tree through its normal configuration (never
/// `--root`, which `vamd` refuses outside `--preflight`). Its stdout is relayed line by line;
/// the key line goes to our stdout and nowhere else.
fn step_vamd_init(root: &Path, vamd: &Path, tls_sans: Option<&str>) -> Result<()> {
    let state = under(root, STATE_DIR);
    let tls_before = state.join("tls/ca.pem").is_file();
    let mut cmd = Command::new(vamd);
    cmd.arg("--init")
        .env("VAMD_DB_PATH", state.join("state.db"))
        .env_remove("VAMD_BOOTSTRAP_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(s) = tls_sans.filter(|s| !s.is_empty()) {
        cmd.env("VAMD_TLS_SANS", s);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("run {} --init", vamd.display()))?;
    let out = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no stdout from vamd --init"))?;
    let mut minted = false;
    let mut existing = false;
    for line in BufReader::new(out).lines() {
        let line = line?;
        minted |= line.starts_with("admin key: ");
        existing |= line.contains("already has an admin key");
        println!("{line}");
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("vamd --init failed ({status}); nothing further was changed");
    }
    if tls_before {
        unchanged("tls", &format!("{}/tls/ca.pem exists", state.display()));
    } else {
        done(
            "tls",
            &format!("vamd minted its certificate under {}/tls", state.display()),
        );
    }
    if minted {
        done("admin-key", "created (shown above, once)");
    } else if existing {
        unchanged("admin-key", "the node already has an admin key");
    }
    Ok(())
}

fn step_state_dir(root: &Path) -> Result<()> {
    let dir = under(root, STATE_DIR);
    if dir.is_dir() {
        unchanged("state-dir", &dir.display().to_string());
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    done("state-dir", &format!("created {} (0700)", dir.display()));
    Ok(())
}

/// Checks init cannot fix (the rest it fixes, or can only judge once the assets are in place).
fn blocking(lines: &[Line]) -> Vec<&Line> {
    lines
        .iter()
        .filter(|l| l.status.needs_attention())
        .filter(|l| {
            !matches!(
                l.check.as_str(),
                "firecracker" | "jailer" | "kernel" | "base-image" | "jailer-rules"
            )
        })
        .collect()
}

fn fail_with(lines: &[&Line], what: &str) -> anyhow::Error {
    let mut s = String::new();
    for l in lines {
        s.push_str(&format!(
            "{} {}\n  {}\n  fix: {}\n",
            l.status.word(),
            l.check,
            l.detail,
            l.fix
        ));
    }
    print!("{s}");
    output::fail(
        ExitCode::Failure,
        format!("{what}; nothing further was changed"),
    )
}

pub async fn run_install(o: Options) -> Result<()> {
    let root = o.root.clone();
    let vamd = find_vamd(o.vamd.as_deref())?;

    let lines = node_init::run_checks(&root, Some(&vamd));
    let stop = blocking(&lines);
    if !stop.is_empty() {
        return Err(fail_with(&stop, "this host cannot run a node"));
    }
    unchanged("checks", "the host can run a node");

    step_state_dir(&root)?;
    step_assets(&root).await?;
    step_base(&root, &o).await?;

    // Everything the node needs is in place: the jailer rules can now be judged for real.
    let after = node_init::run_checks(&root, Some(&vamd));
    let rest: Vec<&Line> = after
        .iter()
        .filter(|l| l.status.needs_attention())
        .collect();
    if !rest.is_empty() {
        return Err(fail_with(
            &rest,
            "the host still fails a check after install",
        ));
    }

    // Keep an existing TLS name list unless --host-name says otherwise.
    let env_path = under(&root, ENV_FILE);
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();
    let sans = o
        .host_name
        .clone()
        .or_else(|| env_value(&existing, "VAMD_TLS_SANS"));
    let env_text = env_file_text(sans.as_deref());
    if write_if_changed(&env_path, env_text.as_bytes(), 0o600)? {
        done("env-file", &format!("wrote {} (0600)", env_path.display()));
    } else {
        unchanged("env-file", &env_path.display().to_string());
    }

    let mut unit_changed = false;
    if o.no_systemd {
        println!("skipped systemd: --no-systemd");
    } else {
        let unit_path = under(&root, UNIT_FILE);
        let text = unit_text(&vamd.display().to_string());
        unit_changed = write_if_changed(&unit_path, text.as_bytes(), 0o644)?;
        if unit_changed {
            done("unit", &format!("wrote {}", unit_path.display()));
        } else {
            unchanged("unit", &unit_path.display().to_string());
        }
    }

    step_vamd_init(&root, &vamd, sans.as_deref())?;

    if !o.no_systemd && !o.no_start && root == Path::new("/") {
        let _ = Command::new("systemctl").arg("daemon-reload").status();
        let active = Command::new("systemctl")
            .args(["is-active", "--quiet", "vamd"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if unit_changed || !active {
            Command::new("systemctl")
                .args(["enable", "--now", "vamd"])
                .status()
                .context("systemctl enable --now vamd")?;
            if unit_changed && active {
                let _ = Command::new("systemctl").args(["restart", "vamd"]).status();
            }
            done("service", "vamd is enabled and started");
        } else {
            unchanged("service", "vamd is enabled and running");
        }
    }

    let host = o
        .host_name
        .clone()
        .unwrap_or_else(|| "localhost".to_string());
    let ca = format!("{}/tls/ca.pem", under(&root, STATE_DIR).display());
    println!();
    println!("To sign in, from this machine or another:");
    println!("  vam config set-host https://{host}:{DEFAULT_PORT}");
    println!(
        "  vam config set-ca {ca}   (copy ca.pem first when you sign in from another machine)"
    );
    println!(
        "  vam config set-key --from-env VAM_API_KEY   (the admin key printed by the first run)"
    );
    println!("  vam node status");
    println!("Or open the app at https://{host}:{DEFAULT_PORT}/ and sign in with the key.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_never_weakens_the_jailer_or_tls() {
        let u = unit_text("/usr/local/bin/vamd");
        let exec = u.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
        assert_eq!(exec, "ExecStart=/usr/local/bin/vamd");
        assert!(u.contains("User=root") && u.contains("EnvironmentFile=/etc/vam/vamd.env"));
        assert!(u.contains("Restart=on-failure"));
        assert!(!u.contains("--no-jailer") && !u.contains("--insecure-dev"));
        assert!(!u.lines().any(|l| l.starts_with("PrivateTmp")));
        let e = env_file_text(Some("node.example"));
        assert!(!e.contains("BOOTSTRAP") && !e.contains("vam_sk"));
    }

    #[test]
    fn write_if_changed_writes_once_and_sets_the_mode() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("etc/vam/vamd.env");
        assert!(write_if_changed(&p, b"A=1\n", 0o600).unwrap());
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!write_if_changed(&p, b"A=1\n", 0o600).unwrap());
        std::fs::write(&p, "edited").unwrap();
        assert!(write_if_changed(&p, b"A=1\n", 0o600).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "A=1\n");
    }

    #[test]
    fn a_wrong_sha256_writes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("vmlinux");
        let err = place(&p, b"kernel", &"0".repeat(64), 0o644).unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"));
        assert!(!p.exists());
        assert!(place(&p, b"kernel", &sha256_bytes(b"kernel"), 0o644).unwrap());
    }

    #[test]
    fn the_env_file_carries_paths_and_an_optional_tls_name() {
        let e = env_file_text(Some("a.example,10.0.0.1"));
        assert_eq!(
            env_value(&e, "VAMD_TLS_SANS").as_deref(),
            Some("a.example,10.0.0.1")
        );
        assert!(env_value(&env_file_text(None), "VAMD_TLS_SANS").is_none());
        assert!(e.contains("VAMD_DB_PATH=/var/lib/vam/state.db"));
    }

    fn tree(r: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(p: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            for e in std::fs::read_dir(p).unwrap().flatten() {
                let q = e.path();
                if q.is_dir() {
                    walk(&q, out)
                } else {
                    out.push((q.clone(), std::fs::read(&q).unwrap()));
                }
            }
        }
        let mut v = Vec::new();
        walk(r, &mut v);
        v.sort();
        v
    }

    #[test]
    fn a_run_installs_then_changes_nothing_and_a_hand_edit_is_repaired() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("root");
        let fx = |rel: &str, body: &str, mode: u32| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        fx("dev/kvm", "", 0o666);
        fx("sys/fs/cgroup/cgroup.controllers", "", 0o644);
        for f in ["firecracker", "jailer"] {
            fx(&format!("var/lib/vam/assets/{f}"), "x", 0o755);
        }
        fx("var/lib/vam/assets/vmlinux", "k", 0o644);
        fx("var/lib/vam/assets/rootfs.ext4", "img", 0o644);
        fx(
            "var/lib/vam/assets/rootfs.base.json",
            r#"{"digest":"sha256:fixture-not-a-real-pin"}"#,
            0o644,
        );
        // A fake vamd: a key the first time, the "already" line after; never --root.
        let vamd = t.path().join("vamd");
        std::fs::write(
            &vamd,
            "#!/bin/sh\n\
case \"$1\" in\n\
--preflight) echo '{\"checks\":[]}';;\n\
--init) d=$(dirname \"$VAMD_DB_PATH\"); mkdir -p $d/tls\n\
  if [ -f $d/state.db ]; then echo 'this node already has an admin key; none shown'\n\
  else echo x > $d/state.db; echo 'admin key: vam_sk_SECRET123'; echo 'save it now; it is not shown again'; fi\n\
  [ -f $d/tls/ca.pem ] || echo ca > $d/tls/ca.pem;;\n\
esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&vamd, std::fs::Permissions::from_mode(0o755)).unwrap();
        let o = || Options {
            root: root.clone(),
            vamd: Some(vamd.clone()),
            no_systemd: false,
            no_start: true,
            base: None,
            base_sha256: None,
            host_name: Some("node.example".into()),
            checkout: None,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run_install(o())).unwrap();
        let unit = root.join("etc/systemd/system/vamd.service");
        assert!(unit.is_file());
        assert_eq!(
            std::fs::metadata(root.join("etc/vam/vamd.env"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        for (_, body) in tree(&root) {
            assert!(!String::from_utf8_lossy(&body).contains("SECRET123"));
        }
        let before = tree(&root);
        rt.block_on(run_install(o())).unwrap();
        assert_eq!(before, tree(&root), "a second run changed a file");
        std::fs::write(&unit, "edited").unwrap();
        rt.block_on(run_install(o())).unwrap();
        assert_eq!(
            std::fs::read_to_string(&unit).unwrap(),
            unit_text(&vamd.display().to_string())
        );
        std::fs::remove_file(&unit).unwrap();
        let mut n = o();
        n.no_systemd = true;
        rt.block_on(run_install(n)).unwrap();
        assert!(!unit.exists());
    }

    #[test]
    fn a_host_without_kvm_stops_with_a_fix_and_changes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let vamd = t.path().join("vamd");
        std::fs::write(&vamd, "#!/bin/sh\necho '{\"checks\":[]}'\n").unwrap();
        std::fs::set_permissions(&vamd, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = t.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let o = Options {
            root: root.clone(),
            vamd: Some(vamd),
            no_systemd: true,
            no_start: true,
            base: None,
            base_sha256: None,
            host_name: None,
            checkout: None,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(rt.block_on(run_install(o)).is_err());
        assert!(tree(&root).is_empty());
        assert!(!root.join("var").exists());
    }
}
