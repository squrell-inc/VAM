//! `vam dock` / `vam undock`: put the running `vam` on the user's `PATH` without admin rights.
//! Linux and macOS only (Windows comes in v0.2.0). Design: `design/release-artifacts.md` §5.7.

use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct DockArgs {
    /// Directory to copy `vam` into (default: ~/.local/bin).
    #[arg(long, value_name = "DIR")]
    pub dir: Option<std::path::PathBuf>,
    /// Print the PATH line instead of adding it to your login profile files.
    #[arg(long)]
    pub no_modify_path: bool,
}

#[derive(Args)]
pub struct UndockArgs {
    /// Directory `vam dock` copied `vam` into (default: ~/.local/bin).
    #[arg(long, value_name = "DIR")]
    pub dir: Option<std::path::PathBuf>,
}

#[cfg(unix)]
pub fn run_dock(args: DockArgs) -> Result<()> {
    let home = unix::home()?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let exe = std::env::current_exe()?;
    let out = unix::dock(
        &home,
        args.dir.as_deref(),
        &path,
        unix::Os::current(),
        args.no_modify_path,
        &exe,
    )?;
    for l in out {
        println!("{l}");
    }
    Ok(())
}

#[cfg(unix)]
pub fn run_undock(args: UndockArgs) -> Result<()> {
    let home = unix::home()?;
    for l in unix::undock(&home, args.dir.as_deref(), unix::Os::current())? {
        println!("{l}");
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn run_dock(_args: DockArgs) -> Result<()> {
    anyhow::bail!("vam dock is not supported on this OS yet (Windows comes in v0.2.0)")
}

#[cfg(not(unix))]
pub fn run_undock(_args: UndockArgs) -> Result<()> {
    anyhow::bail!("vam undock is not supported on this OS yet (Windows comes in v0.2.0)")
}

#[cfg(unix)]
mod unix {
    use anyhow::{bail, Context, Result};
    use sha2::{Digest, Sha256};
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    pub const MARKER: &str = "# added by vam dock";

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Os {
        Linux,
        Macos,
    }

    impl Os {
        pub fn current() -> Os {
            if cfg!(target_os = "macos") {
                Os::Macos
            } else {
                Os::Linux
            }
        }
    }

    pub fn home() -> Result<PathBuf> {
        match std::env::var_os("HOME") {
            Some(h) if !h.is_empty() => Ok(PathBuf::from(h)),
            _ => bail!("HOME is not set; cannot find your login profile files"),
        }
    }

    /// Profile files: (name, created when missing).
    pub fn profile_files(os: Os) -> Vec<(&'static str, bool)> {
        match os {
            Os::Linux => vec![(".profile", true), (".bashrc", false), (".zshrc", false)],
            Os::Macos => vec![(".zprofile", true), (".bash_profile", false)],
        }
    }

    fn default_dir(home: &Path) -> PathBuf {
        home.join(".local").join("bin")
    }

    /// The marker line. The default directory is written as `$HOME/.local/bin`.
    pub fn marker_line(home: &Path, dir: &Path) -> String {
        let shown = if dir == default_dir(home) {
            "$HOME/.local/bin".to_string()
        } else {
            dir.display().to_string()
        };
        format!("export PATH=\"{shown}:$PATH\" {MARKER}")
    }

    fn norm(p: &Path) -> PathBuf {
        fs::canonicalize(p).unwrap_or_else(|_| {
            let s = p.to_string_lossy();
            PathBuf::from(s.trim_end_matches('/').to_string())
        })
    }

    fn on_path(dir: &Path, path: &OsStr) -> bool {
        let d = norm(dir);
        std::env::split_paths(path).any(|e| !e.as_os_str().is_empty() && norm(&e) == d)
    }

    fn digest(p: &Path) -> Option<Vec<u8>> {
        fs::read(p).ok().map(|b| Sha256::digest(b).to_vec())
    }

    /// Returns `true` when the binary was written.
    fn copy_binary(exe: &Path, dest: &Path) -> Result<bool> {
        if let (Ok(a), Ok(b)) = (fs::canonicalize(exe), fs::canonicalize(dest)) {
            if a == b {
                return Ok(false);
            }
        }
        if digest(exe).is_some() && digest(exe) == digest(dest) {
            return Ok(false);
        }
        let parent = dest
            .parent()
            .context("destination has no parent directory")?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let tmp = parent.join(format!(".vam.tmp.{}", std::process::id()));
        fs::copy(exe, &tmp).with_context(|| format!("copying to {}", tmp.display()))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        fs::rename(&tmp, dest).with_context(|| format!("replacing {}", dest.display()))?;
        Ok(true)
    }

    pub fn dock(
        home: &Path,
        dir: Option<&Path>,
        path_env: &OsStr,
        os: Os,
        no_modify: bool,
        exe: &Path,
    ) -> Result<Vec<String>> {
        let dir = dir.map_or_else(|| default_dir(home), Path::to_path_buf);
        let dest = dir.join("vam");
        let mut out = Vec::new();
        let mut changed = false;
        if copy_binary(exe, &dest)? {
            changed = true;
            out.push(format!("copied vam to {}", dest.display()));
        } else {
            out.push(format!("vam unchanged: {}", dest.display()));
        }
        let line = marker_line(home, &dir);
        if no_modify {
            out.push("add this line to your shell profile to put vam on your PATH:".into());
            out.push(line);
        } else if on_path(&dir, path_env) {
            out.push(format!("{} is already on your PATH", dir.display()));
        } else {
            for (name, create) in profile_files(os) {
                let f = home.join(name);
                let existing = match fs::read_to_string(&f) {
                    Ok(s) => Some(s),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(e).with_context(|| format!("reading {}", f.display())),
                };
                if existing.is_none() && !create {
                    continue;
                }
                let mut text = existing.unwrap_or_default();
                if text.lines().any(|l| l == line) {
                    out.push(format!("PATH line unchanged in {}", f.display()));
                    continue;
                }
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&line);
                text.push('\n');
                fs::write(&f, text).with_context(|| format!("writing {}", f.display()))?;
                changed = true;
                out.push(format!("added PATH line to {}", f.display()));
            }
            if changed {
                out.push("open a new terminal to pick it up".into());
            }
        }
        if !changed {
            out.push("unchanged".into());
        }
        Ok(out)
    }

    pub fn undock(home: &Path, dir: Option<&Path>, os: Os) -> Result<Vec<String>> {
        let dir = dir.map_or_else(|| default_dir(home), Path::to_path_buf);
        let dest = dir.join("vam");
        let mut out = Vec::new();
        if fs::symlink_metadata(&dest).is_ok() {
            fs::remove_file(&dest).with_context(|| format!("removing {}", dest.display()))?;
            out.push(format!("removed {}", dest.display()));
        }
        for (name, _) in profile_files(os) {
            let f = home.join(name);
            let Ok(text) = fs::read_to_string(&f) else {
                continue;
            };
            if !text.contains(MARKER) {
                continue;
            }
            let kept: String = text
                .split_inclusive('\n')
                .filter(|l| !l.contains(MARKER))
                .collect();
            fs::write(&f, kept).with_context(|| format!("writing {}", f.display()))?;
            out.push(format!("removed PATH line from {}", f.display()));
        }
        if out.is_empty() {
            out.push("nothing to remove".into());
        }
        Ok(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        struct Env {
            _t: tempfile::TempDir,
            home: PathBuf,
            exe: PathBuf,
        }

        fn env() -> Env {
            let t = tempfile::tempdir().unwrap();
            let home = t.path().join("home");
            fs::create_dir_all(&home).unwrap();
            let exe = t.path().join("src-vam");
            fs::write(&exe, b"binary").unwrap();
            Env { _t: t, home, exe }
        }

        fn d(e: &Env, dir: Option<&Path>, path: &str, os: Os, nm: bool) -> Vec<String> {
            dock(&e.home, dir, OsStr::new(path), os, nm, &e.exe).unwrap()
        }

        fn count(p: &Path) -> usize {
            fs::read_to_string(p).unwrap().matches(MARKER).count()
        }

        #[test]
        fn marker_text() {
            let h = Path::new("/h");
            assert_eq!(
                marker_line(h, &h.join(".local/bin")),
                "export PATH=\"$HOME/.local/bin:$PATH\" # added by vam dock"
            );
            assert_eq!(
                marker_line(h, Path::new("/opt/x")),
                "export PATH=\"/opt/x:$PATH\" # added by vam dock"
            );
        }

        #[test]
        fn dock_once_then_unchanged_and_creates_profile() {
            let e = env();
            let dir = e.home.join(".local/bin");
            let o1 = d(&e, None, "/usr/bin", Os::Linux, false);
            assert!(o1.iter().any(|l| l.starts_with("copied")));
            assert_eq!(count(&e.home.join(".profile")), 1);
            let mode = fs::metadata(dir.join("vam")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
            let o2 = d(&e, None, "/usr/bin", Os::Linux, false);
            assert!(o2.iter().any(|l| l == "unchanged"));
            assert_eq!(count(&e.home.join(".profile")), 1);
        }

        #[test]
        fn existing_content_kept_and_optional_files_only_if_present() {
            let e = env();
            fs::write(e.home.join(".profile"), "export A=1").unwrap();
            fs::write(e.home.join(".zshrc"), "# z\n").unwrap();
            d(&e, None, "/usr/bin", Os::Linux, false);
            let p = fs::read_to_string(e.home.join(".profile")).unwrap();
            assert!(p.starts_with("export A=1\nexport PATH="));
            assert_eq!(count(&e.home.join(".zshrc")), 1);
            assert!(!e.home.join(".bashrc").exists());
        }

        #[test]
        fn macos_files() {
            let e = env();
            fs::write(e.home.join(".bash_profile"), "").unwrap();
            d(&e, None, "/usr/bin", Os::Macos, false);
            assert_eq!(count(&e.home.join(".zprofile")), 1);
            assert_eq!(count(&e.home.join(".bash_profile")), 1);
            assert!(!e.home.join(".profile").exists());
        }

        #[test]
        fn already_on_path_edits_nothing() {
            let e = env();
            let dir = e.home.join(".local/bin");
            let p = format!("/usr/bin:{}/", dir.display());
            let o = d(&e, None, &p, Os::Linux, false);
            assert!(o.iter().any(|l| l.contains("already on your PATH")));
            assert!(!e.home.join(".profile").exists());
        }

        #[test]
        fn no_modify_path_prints_line_and_touches_no_file() {
            let e = env();
            fs::write(e.home.join(".profile"), "x\n").unwrap();
            let o = d(&e, None, "/usr/bin", Os::Linux, true);
            assert!(o.iter().any(|l| l.contains("# added by vam dock")));
            assert_eq!(count(&e.home.join(".profile")), 0);
            assert!(e.home.join(".local/bin/vam").exists());
        }

        #[test]
        fn replaces_existing_binary() {
            let e = env();
            let dir = e.home.join("bin2");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("vam"), b"old").unwrap();
            d(&e, Some(&dir), "/usr/bin", Os::Linux, true);
            assert_eq!(fs::read(dir.join("vam")).unwrap(), b"binary");
        }

        #[test]
        fn undock_removes_only_marked_lines_and_binary() {
            let e = env();
            fs::write(e.home.join(".profile"), "export A=1\n").unwrap();
            d(&e, None, "/usr/bin", Os::Linux, false);
            let o = undock(&e.home, None, Os::Linux).unwrap();
            assert!(o.iter().any(|l| l.starts_with("removed")));
            assert_eq!(
                fs::read_to_string(e.home.join(".profile")).unwrap(),
                "export A=1\n"
            );
            assert!(!e.home.join(".local/bin/vam").exists());
            let o = undock(&e.home, None, Os::Linux).unwrap();
            assert_eq!(o, vec!["nothing to remove".to_string()]);
        }
    }
}
