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
        let what = "the built program runs on the fixture tree";
        let var = x.bin.env_var();
        let mut cli = std::env::var(var).unwrap_or_default();
        if cli.is_empty() {
            return Err(Fail::new(
                Class::Env,
                what,
                format!("{var} is not set (scripts/e2e/run.sh exports it)"),
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
        let built_cli = cli.clone();
        if let Some(rel) = &x.copy_cli {
            let dst = root.join(rel);
            if let Some(dir) = dst.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            std::fs::copy(&cli, &dst).map_err(|e| {
                Fail::new(
                    Class::Harness,
                    "copy the built vam into the tree",
                    format!("{rel}: {e}"),
                )
            })?;
            cli = dst.to_string_lossy().into_owned();
        }
        let secret = self.ctx.cli_secret.clone().unwrap_or_default();
        let vars = self.ctx.cli_vars.clone();
        let sub = |t: &str| {
            let mut t = t
                .replace("{{root}}", &root.to_string_lossy())
                .replace("{{vamd}}", &vamd)
                .replace("{{secret}}", &secret);
            for (k, v) in &vars {
                t = t.replace(&format!("{{{{{k}}}}}"), v);
            }
            t
        };
        let mut args: Vec<String> = x.args.iter().map(|a| sub(a)).collect();
        let name = x.bin.name();
        let mut ca = None;
        if let Some(alias) = &x.as_ {
            let key = self.cli_key(alias)?;
            let mut pre = vec![
                "--host".to_string(),
                self.api.base().to_string(),
                "--token".to_string(),
                key,
            ];
            pre.append(&mut args);
            args = pre;
            ca = self.api.ca_path().map(|p| p.to_path_buf());
        }
        // HOME is beside the tree, not in it, so a step that must leave the tree unchanged can.
        let home = root.with_extension("home");
        let _ = std::fs::create_dir_all(&home);
        let before = x.unchanged.then(|| snapshot(&root));
        let file_before: Vec<(String, Option<Vec<u8>>)> = x
            .files
            .iter()
            .filter(|(_, f)| f.unchanged)
            .map(|(p, _)| (p.clone(), std::fs::read(root.join(p)).ok()))
            .collect();
        let mut cmd = std::process::Command::new(&cli);
        cmd.args(&args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &home)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(ca) = ca {
            cmd.env("VAM_CA_FILE", ca);
        }
        if x.cwd.as_deref() == Some("root") {
            cmd.current_dir(&root);
        }
        let out = run_with_timeout(
            &mut cmd,
            std::time::Duration::from_secs(x.timeout.unwrap_or(30)),
        )
        .map_err(|e| match e {
            RunErr::Spawn(e) => Fail::new(Class::Env, what, format!("{cli}: {e}")),
            RunErr::Timeout => Fail::new(
                Class::Product,
                format!("`{name} ...` exits within {}s", x.timeout.unwrap_or(30)),
                "it did not exit and was killed",
            ),
        })?;
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
        let want = format!("`{} {}` exits {verb}", x.bin.name(), x.args.join(" "));
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
        if let Some(r) = &x.remember {
            let found = text
                .lines()
                .find_map(|l| l.split_once(r.after.as_str()))
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .map(str::to_string);
            match found {
                Some(v) => {
                    self.ctx.cli_vars.insert(r.as_.clone(), v);
                }
                None => {
                    return Err(Fail::new(
                        Class::Product,
                        format!("{want} and prints a word after `{}`", r.after),
                        shown,
                    ))
                }
            }
        }
        let vars = self.ctx.cli_vars.clone();
        let sub = |t: &str| {
            let mut t = t
                .replace("{{root}}", &root.to_string_lossy())
                .replace("{{vamd}}", &vamd)
                .replace("{{secret}}", &secret);
            for (k, v) in &vars {
                t = t.replace(&format!("{{{{{k}}}}}"), v);
            }
            t
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
        for m in &x.matches {
            let re = regex::Regex::new(&sub(m)).map_err(|e| {
                Fail::new(Class::Scenario, "a valid regex in `matches`", e.to_string())
            })?;
            if !re.is_match(&text) {
                return Err(Fail::new(
                    Class::Product,
                    format!("{want} and its output matches `{m}`"),
                    shown,
                ));
            }
        }
        for (path, f) in &x.files {
            let got = std::fs::read(root.join(path)).ok();
            let want_file = format!("`{path}` under the tree");
            if (f.contains.is_some() || f.absent.is_some() || f.unchanged) && got.is_none() {
                return Err(Fail::new(
                    Class::Product,
                    want_file,
                    "the file does not exist",
                ));
            }
            let body = String::from_utf8_lossy(got.as_deref().unwrap_or_default()).into_owned();
            if let Some(c) = &f.contains {
                if !body.contains(&sub(c)) {
                    return Err(Fail::new(
                        Class::Product,
                        format!("{want_file} contains `{c}`"),
                        "it does not",
                    ));
                }
            }
            if let Some(c) = &f.absent {
                if body.contains(&sub(c)) {
                    return Err(Fail::new(
                        Class::Product,
                        format!("{want_file} does not contain `{c}`"),
                        "it does",
                    ));
                }
            }
            if f.unchanged {
                let was = file_before
                    .iter()
                    .find(|(p, _)| p == path)
                    .and_then(|(_, b)| b.clone());
                if was != got {
                    return Err(Fail::new(
                        Class::Product,
                        format!("{want_file} is unchanged by the step"),
                        "its bytes changed",
                    ));
                }
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
        // The binary under test (copied into the tree by `copyCli`, in this step or an earlier
        // one) is not output of the step: the tree scan skips copies of it.
        let placed = std::fs::read(&built_cli).ok();
        for a in x.absent.iter().chain(&x.absent_in_files) {
            if let Some(f) = find_in_tree(&root, sub(a).as_bytes(), placed.as_deref()) {
                return Err(Fail::new(
                    Class::Product,
                    format!("no file under the tree contains `{a}`"),
                    format!("found in {}", f.strip_prefix(&root).unwrap_or(&f).display()),
                ));
            }
        }
        Ok(())
    }

    /// The key `as: <alias>` runs with: the identity's own key, or, for an Owner piece only (the
    /// bootstrap key has no minted identity), the bootstrap key. Any other alias without a key
    /// is a scenario error, never operator privilege.
    pub(super) fn cli_key(&self, alias: &str) -> Result<String, Fail> {
        if let Some(k) = self.key_of(Some(alias))? {
            return Ok(k);
        }
        let owner = self
            .ctx
            .pieces
            .get(alias)
            .and_then(|r| self.pieces.get(&r.piece))
            .is_some_and(|w| is_owner_role(w.role.as_deref()));
        if owner {
            Ok(self.api.key().to_string())
        } else {
            Err(Fail::new(
                Class::Scenario,
                format!("a key for `{alias}`"),
                "none yet",
            ))
        }
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

/// An `org` piece with no role, or `Owner` or `Operator`, is the bootstrap key.
fn is_owner_role(role: Option<&str>) -> bool {
    let norm: String = role
        .unwrap_or("Owner")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    matches!(norm.as_str(), "owner" | "operator")
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

/// The first file under `dir` that contains `needle`, skipping files whose bytes are exactly
/// `skip` (the runner's own copies of the binary under test).
fn find_in_tree(dir: &Path, needle: &[u8], skip: Option<&[u8]>) -> Option<PathBuf> {
    if needle.is_empty() {
        return None;
    }
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        let Ok(t) = e.file_type() else { continue };
        if t.is_dir() {
            if let Some(f) = find_in_tree(&p, needle, skip) {
                return Some(f);
            }
        } else if t.is_file() {
            if let Ok(b) = std::fs::read(&p) {
                if skip == Some(b.as_slice()) {
                    continue;
                }
                if b.windows(needle.len()).any(|w| w == needle) {
                    return Some(p);
                }
            }
        }
    }
    None
}

enum RunErr {
    Spawn(std::io::Error),
    Timeout,
}

/// Runs `cmd` to the end or kills it at `limit`.
fn run_with_timeout(
    cmd: &mut std::process::Command,
    limit: std::time::Duration,
) -> Result<std::process::Output, RunErr> {
    use std::io::Read;
    let mut child = cmd.spawn().map_err(RunErr::Spawn)?;
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let t1 = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(s) = so.as_mut() {
            let _ = s.read_to_end(&mut b);
        }
        b
    });
    let t2 = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(s) = se.as_mut() {
            let _ = s.read_to_end(&mut b);
        }
        b
    });
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait().map_err(RunErr::Spawn)? {
            Some(s) => break s,
            None if start.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunErr::Timeout);
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    Ok(std::process::Output {
        status,
        stdout: t1.join().unwrap_or_default(),
        stderr: t2.join().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tree_scan_skips_copies_of_the_binary_under_test_only() {
        let t = tempfile::tempdir().unwrap();
        let bin = b"\x7fELF the --checkout flag".to_vec();
        std::fs::create_dir_all(t.path().join("kit-1")).unwrap();
        std::fs::write(t.path().join("kit-1/vam"), &bin).unwrap();
        std::fs::write(t.path().join("out.txt"), "done base: built from the kit").unwrap();
        let word = b"checkout";
        assert_eq!(
            find_in_tree(t.path(), word, None),
            Some(t.path().join("kit-1/vam"))
        );
        assert_eq!(find_in_tree(t.path(), word, Some(&bin)), None);
        std::fs::write(t.path().join("out.txt"), "looking for a checkout").unwrap();
        assert_eq!(
            find_in_tree(t.path(), word, Some(&bin)),
            Some(t.path().join("out.txt"))
        );
        // a modified copy is not the binary under test
        std::fs::write(t.path().join("kit-1/vam"), b"other checkout").unwrap();
        std::fs::write(t.path().join("out.txt"), "clean").unwrap();
        assert!(find_in_tree(t.path(), word, Some(&bin)).is_some());
    }

    #[test]
    fn only_an_owner_piece_falls_back_to_the_bootstrap_key() {
        assert!(is_owner_role(None));
        assert!(is_owner_role(Some("Owner")));
        assert!(is_owner_role(Some("operator")));
        assert!(!is_owner_role(Some("Member")));
        assert!(!is_owner_role(Some("OrgAdmin")));
        assert!(!is_owner_role(Some("Publisher")));
    }

    #[test]
    fn a_command_that_hangs_is_killed_at_the_timeout() {
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let t = std::time::Instant::now();
        let r = run_with_timeout(&mut cmd, std::time::Duration::from_millis(200));
        assert!(matches!(r, Err(RunErr::Timeout)));
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_command_that_exits_returns_its_output() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let Ok(o) = run_with_timeout(&mut cmd, std::time::Duration::from_secs(5)) else {
            panic!("did not run");
        };
        assert!(o.status.success());
        assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "out");
        assert_eq!(String::from_utf8_lossy(&o.stderr).trim(), "err");
    }

    #[test]
    fn the_new_cli_fields_parse_and_unknown_ones_are_refused() {
        let ok: CliStep = serde_yaml::from_str(
            "{ args: [--version], root: { from: a }, exit: ok, bin: vamd, cwd: root, \
             matches: ['^x'], remember: { as: v, after: 'vam ' }, as: admin, timeout: 5, \
             copyCli: k/vam, files: { f: { contains: x, unchanged: true } } }",
        )
        .unwrap();
        assert_eq!(ok.bin, crate::scenario::CliBin::Vamd);
        assert_eq!(ok.timeout, Some(5));
        assert_eq!(ok.remember.unwrap().as_, "v");
        assert!(ok.files["f"].unchanged);
        let bad = serde_yaml::from_str::<CliStep>(
            "{ args: [x], root: { from: a }, exit: ok, bin: nonsense }",
        );
        assert!(bad.is_err());
        let bad = serde_yaml::from_str::<CliStep>(
            "{ args: [x], root: { from: a }, exit: ok, home: tree }",
        );
        assert!(bad.is_err());
    }
}
