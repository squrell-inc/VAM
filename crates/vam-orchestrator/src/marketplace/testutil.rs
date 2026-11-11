//! Test support: bare Git repositories made from the e2e fixtures, the way the runner makes them.

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e/world/fixtures")
}

pub fn git_bin() -> Option<PathBuf> {
    super::resolve_git_bin(None)
}

/// A work tree and a bare clone of it under a temp directory.
pub struct TestRepo {
    pub dir: tempfile::TempDir,
    n: u64,
}

fn run(dir: &Path, home: &Path, n: u64, args: &[&str]) -> String {
    let when = format!("@{} +0000", 1_767_225_600 + n);
    let out = Command::new("git")
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.test")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.test")
        .env("GIT_AUTHOR_DATE", &when)
        .env("GIT_COMMITTER_DATE", &when)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args([
            "-c",
            "tag.gpgsign=false",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if e.file_type().unwrap().is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

impl TestRepo {
    /// Commits the fixture directory on `main`, tags that commit with each of `tags`, clones bare.
    pub fn new(fixture: &str, tags: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (work, home) = (dir.path().join("work"), dir.path().join("home"));
        std::fs::create_dir_all(&home).unwrap();
        copy_tree(&fixtures().join(fixture), &work);
        run(&work, &home, 0, &["init", "-q", "-b", "main"]);
        run(&work, &home, 0, &["add", "-A"]);
        run(&work, &home, 0, &["commit", "-q", "-m", "marketplace"]);
        for t in tags {
            run(&work, &home, 0, &["tag", t]);
        }
        run(
            dir.path(),
            &home,
            0,
            &["clone", "-q", "--bare", "work", "bare.git"],
        );
        Self { dir, n: 0 }
    }

    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }
    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    /// `file://` URL of the bare repository.
    pub fn url(&self) -> String {
        format!("file://{}", self.dir.path().join("bare.git").display())
    }

    pub fn head(&self) -> String {
        run(&self.work(), &self.home(), 0, &["rev-parse", "HEAD"])
    }

    pub fn rev(&self, name: &str) -> String {
        run(
            &self.work(),
            &self.home(),
            0,
            &["rev-parse", &format!("{name}^{{commit}}")],
        )
    }

    fn push(&self, refs: &[&str]) {
        let mut args = vec!["push", "-q", "--force", "../bare.git"];
        args.extend_from_slice(refs);
        run(&self.work(), &self.home(), 0, &args);
    }

    /// Copies `overlay` (a fixture directory) over the work tree and commits it on `main`.
    pub fn overlay(&mut self, overlay: &str) {
        copy_tree(&fixtures().join(overlay), &self.work());
        self.commit_all("overlay");
    }

    /// Writes files and commits them on `main`.
    pub fn write(&mut self, files: &[(&str, &str)]) {
        for (p, c) in files {
            let path = self.work().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, c).unwrap();
        }
        self.commit_all("write");
    }

    pub fn commit_all(&mut self, msg: &str) {
        self.n += 1;
        run(&self.work(), &self.home(), self.n, &["add", "-A"]);
        run(
            &self.work(),
            &self.home(),
            self.n,
            &["commit", "-q", "-m", msg],
        );
        self.push(&["HEAD:refs/heads/main"]);
    }

    /// Tags HEAD (moving the tag when it exists) and pushes it.
    pub fn tag(&mut self, tag: &str) {
        run(&self.work(), &self.home(), 0, &["tag", "-f", tag]);
        self.push(&[&format!("refs/tags/{tag}:refs/tags/{tag}")]);
    }

    /// Runs a git command in the work tree (for staging odd objects).
    pub fn git(&self, args: &[&str]) -> String {
        run(&self.work(), &self.home(), 0, args)
    }

    /// Pushes the work tree's `main` and every tag to the bare repository.
    pub fn publish(&self) {
        self.push(&["HEAD:refs/heads/main", "--tags"]);
    }

    pub fn work_dir(&self) -> PathBuf {
        self.work()
    }
}
