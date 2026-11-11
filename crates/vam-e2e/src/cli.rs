//! The `cli` step: runs the built `vam` (`VAM_E2E_CLI`) on a scratch copy of a fixture tree.
//!
//! The command gets no host and no key (an empty environment, `HOME` inside the scratch tree), so
//! it can only act on the tree. `{{root}}` is the scratch tree and `{{vamd}}` the built `vamd`
//! (`VAM_E2E_VAMD`, which `scripts/e2e/run.sh` exports). A missing binary or variable is `env`,
//! a fixture that cannot be copied is `harness`, and an exit or output that is not the one the
//! scenario names is `product`. This module is a child of `engine`; the format is in
//! `scenario.rs`.

use super::{Engine, Fail};
use crate::scenario::{Class, CliExit, CliStep};
use std::path::{Path, PathBuf};

impl Engine {
    /// `cli: { args, root, exit, contains, absent }`
    pub(super) fn cli_step(&mut self, x: &CliStep) -> Result<(), Fail> {
        let what = "the built vam runs on the fixture tree (VAM_E2E_CLI)";
        let cli = std::env::var("VAM_E2E_CLI").unwrap_or_default();
        if cli.is_empty() {
            return Err(Fail::new(
                Class::Env,
                what,
                "VAM_E2E_CLI is not set (scripts/e2e/run.sh exports it)",
            ));
        }
        let vamd = std::env::var("VAM_E2E_VAMD").unwrap_or_default();
        let wants_vamd = x.args.iter().any(|a| a.contains("{{vamd}}"));
        if wants_vamd && vamd.is_empty() {
            return Err(Fail::new(
                Class::Env,
                what,
                "VAM_E2E_VAMD is not set (scripts/e2e/run.sh exports it)",
            ));
        }
        let root = self.cli_tree(x, what)?;
        let secret = self.ctx.cli_secret.clone().unwrap_or_default();
        let sub = |t: &str| {
            t.replace("{{root}}", &root.to_string_lossy())
                .replace("{{vamd}}", &vamd)
                .replace("{{secret}}", &secret)
        };
        let args: Vec<String> = x.args.iter().map(|a| sub(a)).collect();
        // HOME is beside the tree, not in it, so a step that must leave the tree unchanged can.
        let home = root.with_extension("home");
        let _ = std::fs::create_dir_all(&home);
        let before = x.unchanged.then(|| snapshot(&root));
        let out = std::process::Command::new(&cli)
            .args(&args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &home)
            .output()
            .map_err(|e| Fail::new(Class::Env, what, format!("{cli}: {e}")))?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let text = text.trim().to_string();
        let shown = format!(
            "exit {}: {}",
            out.status.code().map_or("signal".into(), |c| c.to_string()),
            self.scrub(&text.chars().take(600).collect::<String>())
        );
        let verb = match x.exit {
            CliExit::Ok => "ok",
            CliExit::Fail => "non-zero",
        };
        let want = format!("`vam {}` exits {verb}", x.args.join(" "));
        if out.status.success() != (x.exit == CliExit::Ok) {
            return Err(Fail::new(Class::Product, want, shown));
        }
        if let Some(prefix) = &x.capture {
            let found = text
                .lines()
                .find_map(|l| l.split_once(prefix.as_str()))
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .map(str::to_string);
            match found {
                Some(v) => self.ctx.cli_secret = Some(v),
                None => {
                    return Err(Fail::new(
                        Class::Product,
                        format!("{want} and prints a line with `{prefix}` and a value"),
                        shown,
                    ))
                }
            }
        }
        // A secret captured by this step is known from here on.
        let secret = self.ctx.cli_secret.clone().unwrap_or_default();
        let sub = |t: &str| {
            t.replace("{{root}}", &root.to_string_lossy())
                .replace("{{vamd}}", &vamd)
                .replace("{{secret}}", &secret)
        };
        if let Some(before) = before {
            if before != snapshot(&root) {
                return Err(Fail::new(
                    Class::Product,
                    format!("`vam {}` leaves the tree unchanged", x.args.join(" ")),
                    "a file under the tree was added, removed or changed",
                ));
            }
        }
        if x.contains
            .iter()
            .chain(&x.absent)
            .chain(&x.absent_in_files)
            .any(|t| sub(t).is_empty())
        {
            return Err(Fail::new(
                Class::Scenario,
                what,
                "a `contains` or `absent` entry is empty after substitution (`{{secret}}` with no capture?)",
            ));
        }
        // Messages name the template (`{{secret}}`), never the substituted value.
        for c in &x.contains {
            if !text.contains(&sub(c)) {
                return Err(Fail::new(
                    Class::Product,
                    format!("{want} and its output contains `{c}`"),
                    shown,
                ));
            }
        }
        for a in &x.absent {
            if text.contains(&sub(a)) {
                return Err(Fail::new(
                    Class::Product,
                    format!(
                        "the output of `vam {}` does not contain `{a}`",
                        x.args.join(" ")
                    ),
                    "the text is in the output (not shown)",
                ));
            }
        }
        for a in x.absent.iter().chain(&x.absent_in_files) {
            if let Some(f) = find_in_tree(&root, sub(a).as_bytes()) {
                return Err(Fail::new(
                    Class::Product,
                    format!("no file under the tree contains `{a}`"),
                    format!("found in {}", f.strip_prefix(&root).unwrap_or(&f).display()),
                ));
            }
        }
        Ok(())
    }

    /// The scratch tree for this step: a fresh copy of `from`, or the previous step's tree.
    fn cli_tree(&mut self, x: &CliStep, what: &str) -> Result<PathBuf, Fail> {
        if x.root.same {
            return self.ctx.cli_root.clone().ok_or_else(|| {
                Fail::new(
                    Class::Scenario,
                    what,
                    "`root: same` with no earlier `cli` step",
                )
            });
        }
        let from = x.root.from.clone().unwrap_or_default();
        let src = Path::new("e2e").join("world").join("fixtures").join(&from);
        let dir = std::env::temp_dir().join(format!(
            "vam-e2e-cli-{}-{}",
            self.cfg.tag.0,
            std::process::id()
        ));
        make_writable(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        copy_tree(&src, &dir).map_err(|e| {
            Fail::new(
                Class::Harness,
                format!("copy fixture tree `{from}`"),
                format!("{}: {e}", src.display()),
            )
        })?;
        self.ctx.cli_root = Some(dir.clone());
        Ok(dir)
    }
}

fn make_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(rd) = std::fs::read_dir(dir) {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        for e in rd.flatten() {
            if e.path().is_dir() {
                make_writable(&e.path());
            }
        }
    }
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

/// Every file under `dir` with its bytes, by relative path.
fn snapshot(dir: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap_or(&p).to_path_buf();
            match e.file_type() {
                Ok(t) if t.is_dir() => {
                    out.insert(rel, Vec::new());
                    walk(base, &p, out);
                }
                _ => {
                    out.insert(rel, std::fs::read(&p).unwrap_or_default());
                }
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// The first file under `dir` whose bytes contain `needle`.
fn find_in_tree(dir: &Path, needle: &[u8]) -> Option<PathBuf> {
    if needle.is_empty() {
        return None;
    }
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        let Ok(t) = e.file_type() else { continue };
        if t.is_dir() {
            if let Some(f) = find_in_tree(&p, needle) {
                return Some(f);
            }
        } else if t.is_file() {
            if let Ok(b) = std::fs::read(&p) {
                if b.windows(needle.len()).any(|w| w == needle) {
                    return Some(p);
                }
            }
        }
    }
    None
}
