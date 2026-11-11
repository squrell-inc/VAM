//! `vam node init --check`: what a host needs before it can run a VAM node, one line each
//! (workstream `machine-base`, MB-1.8).
//!
//! The command needs no host and no key, reads the machine and changes nothing. `--root DIR`
//! is a test seam: every host path (`/dev/kvm`, the cgroup mount, the asset directory) is
//! looked up under DIR, and the real root is never touched. The jailer-specific rules are not
//! copied here: they come from `vamd --preflight`, so this crate takes no `vam-core`
//! dependency and the rules exist once. No fix ever offers to switch the jailer off or to
//! weaken TLS.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::output::{self, ExitCode};

/// The pins (`scripts/assets.pins`), the file `firecracker-assets.sh` also sources.
const PINS: &str = include_str!("../../../../scripts/assets.pins");

/// Where the node's assets live, under the root.
const ASSETS: &str = "var/lib/vam/assets";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Missing,
    Wrong,
    Skipped,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Missing => "missing",
            Status::Wrong => "wrong",
            Status::Skipped => "skipped",
        }
    }
    pub fn needs_attention(self) -> bool {
        matches!(self, Status::Missing | Status::Wrong)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Line {
    pub check: String,
    pub status: Status,
    pub detail: String,
    pub fix: String,
}

fn ok(check: &str, detail: impl Into<String>) -> Line {
    Line {
        check: check.to_string(),
        status: Status::Ok,
        detail: detail.into(),
        fix: String::new(),
    }
}

fn bad(check: &str, status: Status, detail: impl Into<String>, fix: impl Into<String>) -> Line {
    Line {
        check: check.to_string(),
        status,
        detail: detail.into(),
        fix: fix.into(),
    }
}

/// `KEY="value"` from the embedded pins.
pub fn pin_value(key: &str) -> String {
    pin(key)
}

fn pin(key: &str) -> String {
    PINS.lines()
        .find_map(|l| {
            let v = l.strip_prefix(key)?.strip_prefix('=')?;
            Some(v.trim().trim_matches('"').to_string())
        })
        .unwrap_or_default()
}

fn under(root: &Path, abs: &str) -> PathBuf {
    root.join(abs.trim_start_matches('/'))
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The asset directory's own `assets.pins` (written at install): `name=sha256` per asset. A
/// value that is not a sha256 cannot be compared; the line says so instead of passing quietly.
fn recorded_pin(assets: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(assets.join("assets.pins")).ok()?;
    text.lines().find_map(|l| {
        let v = l.strip_prefix(name)?.strip_prefix('=')?;
        Some(v.trim().to_ascii_lowercase())
    })
}

/// A file the node needs: present, non-empty, executable when asked, and equal to its recorded
/// sha256 when there is one.
fn asset(root: &Path, check: &str, file: &str, executable: bool, fix: &str) -> Line {
    let assets = under(root, ASSETS);
    let path = assets.join(file);
    let meta = match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => m,
        _ => {
            return bad(
                check,
                Status::Missing,
                format!("{} is not there", path.display()),
                fix,
            )
        }
    };
    if meta.len() == 0 {
        return bad(
            check,
            Status::Wrong,
            format!("{} is empty", path.display()),
            fix,
        );
    }
    if executable && meta.permissions().mode() & 0o111 == 0 {
        return bad(
            check,
            Status::Wrong,
            format!("{} is not executable", path.display()),
            format!("chmod 755 {}, or {}", path.display(), fix),
        );
    }
    match recorded_pin(&assets, check) {
        Some(p) if is_sha256_hex(&p) => match sha256_file(&path) {
            Ok(actual) if actual == p => ok(check, format!("{} (sha256 matches)", path.display())),
            Ok(actual) => bad(
                check,
                Status::Wrong,
                format!(
                    "{} has sha256 {actual}, the node pinned {p}",
                    path.display()
                ),
                fix,
            ),
            Err(e) => bad(
                check,
                Status::Wrong,
                format!("{} cannot be read: {e}", path.display()),
                fix,
            ),
        },
        Some(_) => ok(
            check,
            format!(
                "{} (recorded pin is not a sha256: not compared)",
                path.display()
            ),
        ),
        None => ok(check, format!("{} (no recorded pin)", path.display())),
    }
}

fn check_kvm(root: &Path) -> Line {
    let path = under(root, "/dev/kvm");
    if !path.exists() {
        return bad(
            "kvm",
            Status::Missing,
            format!("{} does not exist", path.display()),
            "use a host with hardware virtualization and load the kvm module (modprobe kvm_intel or kvm_amd); on a cloud VM, enable nested virtualization",
        );
    }
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(_) => ok(
            "kvm",
            format!("{} is readable and writable", path.display()),
        ),
        Err(e) => bad(
            "kvm",
            Status::Wrong,
            format!("{} cannot be opened read-write: {e}", path.display()),
            "run as root, or add the account that runs vamd to the kvm group",
        ),
    }
}

fn check_cgroup(root: &Path) -> Line {
    let path = under(root, "/sys/fs/cgroup/cgroup.controllers");
    if path.is_file() {
        ok("cgroup-v2", format!("{} exists", path.display()))
    } else {
        bad(
            "cgroup-v2",
            Status::Missing,
            format!("{} does not exist: this host is not on the unified cgroup v2 hierarchy", path.display()),
            "boot with systemd.unified_cgroup_hierarchy=1 (the jailer limits each VM through cgroup v2)",
        )
    }
}

fn check_platform() -> Line {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        ok("platform", "x86_64 Linux")
    } else {
        bad(
            "platform",
            Status::Wrong,
            "this is not x86_64 Linux; the pinned Firecracker and guest kernel are x86_64",
            "run the node on an x86_64 Linux host",
        )
    }
}

/// The base manifest beside the image: `<image>.base.json`, the rule the node itself uses.
fn check_base(root: &Path) -> Line {
    let fix = "run `vam node init` to put the base in place (it builds it, or takes --base)";
    let assets = under(root, ASSETS);
    let image = assets.join("rootfs.ext4");
    let manifest = assets.join("rootfs.base.json");
    if !image.is_file() {
        return bad(
            "base-image",
            Status::Missing,
            format!("{} is not there", image.display()),
            fix,
        );
    }
    let text = match std::fs::read_to_string(&manifest) {
        Ok(t) => t,
        Err(_) => {
            return bad(
                "base-image",
                Status::Missing,
                format!(
                    "{} is not there: without it the node reports no runtimes",
                    manifest.display()
                ),
                fix,
            )
        }
    };
    let digest = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v["digest"].as_str().map(str::to_string))
        .filter(|d| !d.trim().is_empty());
    let Some(digest) = digest else {
        return bad(
            "base-image",
            Status::Wrong,
            format!("{} has no readable digest", manifest.display()),
            fix,
        );
    };
    let want = digest
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(digest.trim())
        .to_ascii_lowercase();
    if !is_sha256_hex(&want) {
        return ok(
            "base-image",
            format!(
                "{} (manifest digest is not a sha256: not compared)",
                image.display()
            ),
        );
    }
    match sha256_file(&image) {
        Ok(actual) if actual == want => ok(
            "base-image",
            format!("{} (digest matches its manifest)", image.display()),
        ),
        Ok(actual) => bad(
            "base-image",
            Status::Wrong,
            format!(
                "{} has sha256 {actual}, its manifest says {want}",
                image.display()
            ),
            fix,
        ),
        Err(e) => bad(
            "base-image",
            Status::Wrong,
            format!("{} cannot be read: {e}", image.display()),
            fix,
        ),
    }
}

/// `vamd --preflight`, run with the standard asset paths in its environment (unless the
/// caller already set them) and `--root` when a root is given.
fn vamd_lines(root: &Path, vamd: Option<&Path>) -> Vec<Line> {
    let missing = |detail: String| {
        vec![bad(
            "vamd",
            Status::Missing,
            detail,
            "install vamd next to vam, or pass --vamd PATH",
        )]
    };
    let vamd = match vamd {
        Some(p) => p.to_path_buf(),
        None => match std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("vamd")))
            .filter(|p| p.is_file())
        {
            Some(p) => p,
            None => return missing("no vamd binary found beside vam".to_string()),
        },
    };
    let mut cmd = Command::new(&vamd);
    cmd.arg("--preflight");
    if root != Path::new("/") {
        cmd.arg("--root").arg(root);
        if root.is_dir() {
            cmd.current_dir(root);
        }
    }
    for (var, file) in [
        ("VAMD_FIRECRACKER_BIN", "firecracker"),
        ("VAMD_JAILER_BIN", "jailer"),
        ("VAM_ROOTFS_PATH", "rootfs.ext4"),
    ] {
        if std::env::var_os(var).is_none() {
            cmd.env(var, format!("/{ASSETS}/{file}"));
        }
    }
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) => return missing(format!("{} cannot be run: {e}", vamd.display())),
    };
    let parsed: Option<serde_json::Value> = serde_json::from_slice(&out.stdout).ok();
    let Some(checks) = parsed.as_ref().and_then(|v| v["checks"].as_array()) else {
        return vec![bad(
            "vamd",
            Status::Wrong,
            format!("{} --preflight printed no report", vamd.display()),
            "use a vamd built from the same release as vam",
        )];
    };
    checks
        .iter()
        .map(|c| {
            let text = |k: &str| c[k].as_str().unwrap_or_default().to_string();
            let status = match c["status"].as_str() {
                Some("ok") => Status::Ok,
                Some("skipped") => Status::Skipped,
                Some("missing") => Status::Missing,
                _ => Status::Wrong,
            };
            Line {
                check: text("name"),
                status,
                detail: text("detail"),
                fix: text("fix"),
            }
        })
        .collect()
}

/// Every check, in the order they print. Writes nothing.
pub fn run_checks(root: &Path, vamd: Option<&Path>) -> Vec<Line> {
    let mut lines = run_checks_base(root);
    lines.extend(vamd_lines(root, vamd));
    lines
}

fn run_checks_base(root: &Path) -> Vec<Line> {
    vec![
        check_platform(),
        check_kvm(root),
        check_cgroup(root),
        asset(
            root,
            "firecracker",
            "firecracker",
            true,
            &format!(
                "run `vam node init` to fetch Firecracker {} (sha256-verified)",
                pin("FC_VERSION")
            ),
        ),
        asset(
            root,
            "jailer",
            "jailer",
            true,
            &format!(
                "run `vam node init` to fetch the jailer {} (it ships with Firecracker)",
                pin("FC_VERSION")
            ),
        ),
        asset(
            root,
            "kernel",
            "vmlinux",
            false,
            &format!(
                "run `vam node init` to fetch the guest kernel {} (sha256-verified)",
                pin("KERNEL_NAME")
            ),
        ),
        check_base(root),
    ]
}

/// The asset checks alone (firecracker, jailer, kernel), for the installer to decide what to fetch.
pub fn run_asset_checks(root: &Path) -> Vec<Line> {
    run_checks_without_vamd(root)
}

fn run_checks_without_vamd(root: &Path) -> Vec<Line> {
    let mut v = run_checks_base(root);
    v.retain(|l| matches!(l.check.as_str(), "firecracker" | "jailer" | "kernel"));
    v
}

/// Whether the base image and its manifest pass the same check `--check` applies.
pub fn base_is_ok(root: &Path) -> bool {
    check_base(root).status == Status::Ok
}

pub fn render(lines: &[Line]) -> String {
    let mut s = String::new();
    for l in lines {
        s.push_str(&format!("{} {}\n", l.status.word(), l.check));
        if !l.detail.is_empty() {
            s.push_str(&format!("  {}\n", l.detail));
        }
        if l.status.needs_attention() && !l.fix.is_empty() {
            s.push_str(&format!("  fix: {}\n", l.fix));
        }
    }
    s
}

/// `vam node init --check`. Exit 1 when anything is missing or wrong.
pub fn run_check(root: Option<PathBuf>, vamd: Option<PathBuf>) -> Result<()> {
    let root = root.unwrap_or_else(|| PathBuf::from("/"));
    let lines = run_checks(&root, vamd.as_deref());
    let failing = lines.iter().filter(|l| l.status.needs_attention()).count();
    if output::is_json() {
        println!("{}", serde_json::to_string(&lines)?);
    } else {
        print!("{}", render(&lines));
    }
    if failing > 0 {
        return Err(output::fail(
            ExitCode::Failure,
            format!(
                "{failing} of {} checks need attention; nothing was changed",
                lines.len()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write(root: &Path, rel: &str, body: &str, mode: u32) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn find<'a>(lines: &'a [Line], name: &str) -> &'a Line {
        lines.iter().find(|l| l.check == name).unwrap()
    }

    fn ready(root: &Path) {
        write(root, "dev/kvm", "x", 0o644);
        write(root, "sys/fs/cgroup/cgroup.controllers", "cpu\n", 0o644);
        write(root, "var/lib/vam/assets/firecracker", "#!/bin/sh\n", 0o755);
        write(root, "var/lib/vam/assets/jailer", "#!/bin/sh\n", 0o755);
        write(root, "var/lib/vam/assets/vmlinux", "kernel", 0o644);
        write(root, "var/lib/vam/assets/rootfs.ext4", "image", 0o644);
        write(
            root,
            "var/lib/vam/assets/rootfs.base.json",
            r#"{"digest":"sha256:fixture-not-a-real-pin"}"#,
            0o644,
        );
    }

    #[test]
    fn a_bare_tree_names_every_missing_thing_with_a_fix() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("dev")).unwrap();
        std::fs::create_dir_all(t.path().join("sys/fs/cgroup/memory")).unwrap();
        let lines = vec![
            check_kvm(t.path()),
            check_cgroup(t.path()),
            asset(t.path(), "firecracker", "firecracker", true, "f"),
            asset(t.path(), "jailer", "jailer", true, "f"),
            asset(t.path(), "kernel", "vmlinux", false, "f"),
            check_base(t.path()),
        ];
        for l in &lines {
            assert_eq!(l.status, Status::Missing, "{}", l.check);
            assert!(!l.fix.is_empty(), "{}", l.check);
        }
        let text = render(&lines);
        for name in [
            "kvm",
            "cgroup-v2",
            "firecracker",
            "jailer",
            "kernel",
            "base-image",
        ] {
            assert!(text.contains(&format!("missing {name}\n")), "{text}");
        }
        assert!(text.contains("fix:"));
    }

    #[test]
    fn a_ready_tree_passes_every_file_check() {
        let t = tempfile::tempdir().unwrap();
        ready(t.path());
        assert_eq!(check_kvm(t.path()).status, Status::Ok);
        assert_eq!(check_cgroup(t.path()).status, Status::Ok);
        for (n, f, x) in [
            ("firecracker", "firecracker", true),
            ("jailer", "jailer", true),
            ("kernel", "vmlinux", false),
        ] {
            assert_eq!(asset(t.path(), n, f, x, "f").status, Status::Ok, "{n}");
        }
        assert_eq!(check_base(t.path()).status, Status::Ok);
    }

    #[test]
    fn a_binary_that_is_not_executable_is_wrong() {
        let t = tempfile::tempdir().unwrap();
        ready(t.path());
        write(t.path(), "var/lib/vam/assets/jailer", "#!/bin/sh\n", 0o644);
        let l = asset(t.path(), "jailer", "jailer", true, "f");
        assert_eq!(l.status, Status::Wrong);
    }

    #[test]
    fn a_recorded_sha256_that_does_not_match_is_wrong_and_one_that_does_is_ok() {
        let t = tempfile::tempdir().unwrap();
        ready(t.path());
        let real = sha256_file(&t.path().join("var/lib/vam/assets/vmlinux")).unwrap();
        write(
            t.path(),
            "var/lib/vam/assets/assets.pins",
            &format!("kernel={real}\n"),
            0o644,
        );
        assert_eq!(
            asset(t.path(), "kernel", "vmlinux", false, "f").status,
            Status::Ok
        );
        write(
            t.path(),
            "var/lib/vam/assets/assets.pins",
            &format!("kernel={}\n", "0".repeat(64)),
            0o644,
        );
        assert_eq!(
            asset(t.path(), "kernel", "vmlinux", false, "f").status,
            Status::Wrong
        );
    }

    #[test]
    fn the_base_digest_is_compared_when_it_is_a_sha256() {
        let t = tempfile::tempdir().unwrap();
        ready(t.path());
        let real = sha256_file(&t.path().join("var/lib/vam/assets/rootfs.ext4")).unwrap();
        let manifest = |d: &str| format!(r#"{{"digest":"sha256:{d}"}}"#);
        write(
            t.path(),
            "var/lib/vam/assets/rootfs.base.json",
            &manifest(&real),
            0o644,
        );
        assert_eq!(check_base(t.path()).status, Status::Ok);
        write(
            t.path(),
            "var/lib/vam/assets/rootfs.base.json",
            &manifest(&"1".repeat(64)),
            0o644,
        );
        assert_eq!(check_base(t.path()).status, Status::Wrong);
    }

    #[test]
    fn a_missing_manifest_or_digest_is_named() {
        let t = tempfile::tempdir().unwrap();
        ready(t.path());
        std::fs::remove_file(t.path().join("var/lib/vam/assets/rootfs.base.json")).unwrap();
        assert_eq!(check_base(t.path()).status, Status::Missing);
        write(t.path(), "var/lib/vam/assets/rootfs.base.json", "{}", 0o644);
        assert_eq!(check_base(t.path()).status, Status::Wrong);
    }

    #[test]
    fn the_pins_are_the_ones_firecracker_assets_sh_sources() {
        assert_eq!(pin("FC_VERSION"), "v1.16.1");
        assert!(is_sha256_hex(&pin("KERNEL_SHA256")));
    }

    #[test]
    fn no_fix_offers_to_weaken_the_jailer_or_tls() {
        let t = tempfile::tempdir().unwrap();
        let lines = run_checks(t.path(), Some(Path::new("/nonexistent/vamd")));
        let text = render(&lines).to_lowercase();
        for banned in ["no-jailer", "use_jailer=0", "insecure", "plain http"] {
            assert!(!text.contains(banned), "{banned} in {text}");
        }
        assert_eq!(find(&lines, "vamd").status, Status::Missing);
    }

    #[test]
    fn checking_changes_nothing_under_the_root() {
        let t = tempfile::tempdir().unwrap();
        ready(t.path());
        let before = format!("{:?}", walk(t.path()));
        let _ = run_checks(t.path(), None);
        assert_eq!(before, format!("{:?}", walk(t.path())));
    }

    fn walk(dir: &Path) -> Vec<(PathBuf, u64)> {
        let mut v = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                v.extend(walk(&p));
            } else {
                v.push((p.clone(), std::fs::metadata(&p).unwrap().len()));
            }
        }
        v.sort();
        v
    }
}
