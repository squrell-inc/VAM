//! Reading a Git marketplace without running anything from it.
//!
//! vamd shells out to the `git` binary, but only for a handful of read operations and only after
//! hardening it so that nothing in a repository (or in the machine's own Git configuration) can
//! make it run code, prompt, follow a redirect, or reach anything but a public https host:
//!
//! - the environment is cleared; `HOME` is an empty directory of vamd's own (no `.netrc`,
//!   no credential files); `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`,
//!   `GIT_TERMINAL_PROMPT=0`; nothing is passed through from vamd's environment;
//! - every call carries `core.hooksPath=/dev/null`, `protocol.allow=never` with
//!   `protocol.https.allow=always`, `http.followRedirects=false`, `fetch.fsckObjects=true`, no
//!   submodule recursion, and (for https) `http.proxy` set to the guard proxy, which dials public
//!   addresses on port 443 only ([`super::guard`]);
//! - the repository is a bare cache under vamd's state directory. Nothing is ever checked out, so
//!   no smudge or clean filter, LFS pointer, attribute or hook can run; blobs are read raw with
//!   `git cat-file` and written by vamd itself, refusing symlinks, submodules and odd paths;
//! - arguments go as argv (no shell); URLs, refs and object ids are validated first and come after
//!   `--` wherever Git accepts it;
//! - each call has a wall-clock timeout, an output cap, and (for a fetch) a cap on how far the
//!   cache may grow; a call that exceeds one is killed together with its child processes.
//!
//! The `file://` transport exists only in test builds and in a `process-guest` vamd, where the
//! runner's repositories are mapped to the https URL the node is told about.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use vam_spec::marketplace::check_git_ref;
use vam_spec::package::MAX_PACKAGE_FILE_BYTES;

use super::url::{check_host_public_system, MarketplaceUrl, UrlError};

/// Where a repository is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remote {
    /// An `https://` URL that passed [`super::url::check_url`].
    Https { url: String, host: String },
    /// A local bare repository standing in for an https URL (tests and process-guest nodes).
    #[cfg(any(test, feature = "process-guest"))]
    File { url: String },
}

impl Remote {
    /// The remote for a checked marketplace URL. A `process-guest` node may map it to a local
    /// repository; every other node reads the https URL.
    pub fn for_url(u: &MarketplaceUrl) -> Remote {
        #[cfg(test)]
        if let Some(file) = TEST_SEAM.with(|m| m.borrow().get(&u.canonical).cloned()) {
            return Remote::File { url: file };
        }
        #[cfg(feature = "process-guest")]
        if let Some(file) = crate::process_mode::map_marketplace_url(&u.canonical) {
            return Remote::File { url: file };
        }
        Remote::Https {
            url: u.canonical.clone(),
            host: u.host.clone(),
        }
    }

    fn url(&self) -> &str {
        match self {
            Remote::Https { url, .. } => url,
            #[cfg(any(test, feature = "process-guest"))]
            Remote::File { url } => url,
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Test builds map an https URL to a local bare repository, as the process seam does.
    pub(crate) static TEST_SEAM: std::cell::RefCell<BTreeMap<String, String>> =
        const { std::cell::RefCell::new(BTreeMap::new()) };
}

/// Why a read of a marketplace failed. The text is shown to the admin (and kept as `lastError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    #[error("git is not installed on this node (install git, or set VAMD_GIT_BIN to its path)")]
    GitMissing,
    #[error("{0}")]
    Refused(String),
    #[error("git {0} timed out")]
    Timeout(&'static str),
    #[error("{0}")]
    TooLarge(String),
    #[error("git failed: {0}")]
    Git(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Io(String),
}

impl From<UrlError> for FetchError {
    fn from(e: UrlError) -> Self {
        FetchError::Refused(e.to_string())
    }
}

/// Size and time caps.
#[derive(Debug, Clone)]
pub struct Limits {
    pub ls_remote: Duration,
    pub fetch: Duration,
    pub local: Duration,
    /// Output of `ls-remote` (the tag list).
    pub max_ls_bytes: usize,
    /// How far the bare cache of one marketplace may grow during a fetch, and when it is wiped.
    pub max_cache_bytes: u64,
    pub max_tags: usize,
    /// One exported artifact directory.
    pub max_export_bytes: u64,
    pub max_export_files: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            ls_remote: Duration::from_secs(30),
            fetch: Duration::from_secs(120),
            local: Duration::from_secs(20),
            max_ls_bytes: 4 * 1024 * 1024,
            max_cache_bytes: 128 * 1024 * 1024,
            max_tags: 20_000,
            max_export_bytes: 16 * 1024 * 1024,
            max_export_files: 2000,
        }
    }
}

/// The refs a remote advertises: its default branch head and its tags (peeled to commits).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteRefs {
    pub head: String,
    pub tags: BTreeMap<String, String>,
}

/// One file of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub kind: String,
    pub oid: String,
    pub size: u64,
    pub path: String,
}

/// A marketplace id is `[a-z0-9-]{1,40}`; it names a directory, so nothing else is accepted.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
}

/// A 40-character lowercase hex object id.
pub fn valid_oid(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The destination ref of a fetched tag: flat, so two tags can never collide as file and
/// directory (`a` and `a/b`).
pub fn tag_ref(tag: &str) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(tag.as_bytes());
    let hex: String = h.iter().take(12).map(|b| format!("{b:02x}")).collect();
    format!("refs/vam/t/{hex}")
}

pub const HEAD_REF: &str = "refs/vam/head";

/// Arguments Git is given in front of every subcommand.
pub(crate) fn config_args(remote: Option<&Remote>, proxy: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = [
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "core.askPass=",
        "credential.helper=",
        "protocol.allow=never",
        "protocol.version=2",
        "http.followRedirects=false",
        "http.proxyAuthMethod=basic",
        "http.sslVerify=true",
        "http.lowSpeedLimit=1000",
        "http.lowSpeedTime=30",
        "transfer.fsckObjects=true",
        "fetch.fsckObjects=true",
        "fetch.recurseSubmodules=false",
        "submodule.recurse=false",
        "gc.auto=0",
        "advice.detachedHead=false",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    match remote {
        Some(Remote::Https { .. }) => {
            a.push("protocol.https.allow=always".into());
            if let Some(p) = proxy {
                a.push(format!("http.proxy={p}"));
            }
        }
        #[cfg(any(test, feature = "process-guest"))]
        Some(Remote::File { .. }) => a.push("protocol.file.allow=always".into()),
        None => {}
    }
    a.into_iter().flat_map(|v| ["-c".to_string(), v]).collect()
}

/// The whole environment Git runs with.
pub(crate) fn child_env(
    git_bin: &Path,
    home: &Path,
    tmp: &Path,
    remote: Option<&Remote>,
) -> Vec<(String, String)> {
    let bin_dir = git_bin
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let allow = match remote {
        Some(Remote::Https { .. }) => "https",
        #[cfg(any(test, feature = "process-guest"))]
        Some(Remote::File { .. }) => "file",
        None => "none",
    };
    [
        ("PATH", format!("{bin_dir}:/usr/bin:/bin")),
        ("HOME", home.display().to_string()),
        ("TMPDIR", tmp.display().to_string()),
        ("LC_ALL", "C".into()),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
        ("GIT_CONFIG_SYSTEM", "/dev/null".into()),
        ("GIT_TERMINAL_PROMPT", "0".into()),
        ("GIT_ALLOW_PROTOCOL", allow.into()),
        ("GIT_LITERAL_PATHSPECS", "1".into()),
        ("GIT_NO_REPLACE_OBJECTS", "1".into()),
        ("GIT_OPTIONAL_LOCKS", "0".into()),
        ("GIT_LFS_SKIP_SMUDGE", "1".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

pub(crate) fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(rd) = std::fs::read_dir(path) else {
        return 0;
    };
    for e in rd.flatten() {
        let Ok(meta) = e.path().symlink_metadata() else {
            continue;
        };
        if meta.is_dir() {
            total += dir_size(&e.path());
        } else {
            total += meta.len();
        }
    }
    total
}

struct Call<'a> {
    git_dir: Option<&'a Path>,
    remote: Option<&'a Remote>,
    args: Vec<OsString>,
    timeout: Duration,
    what: &'static str,
    max_stdout: usize,
    /// A directory whose size is watched; the call is killed when it grows past the cap.
    watch: Option<(&'a Path, u64)>,
}

/// Kills a process group.
fn kill_group(pid: u32) {
    // SAFETY: plain `kill(2)` on the child's own process group (it was started with
    // `process_group(0)`, so the group id is the child's pid).
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// What reads and runs Git for one node.
#[derive(Debug, Clone)]
pub struct Fetcher {
    /// The `git` executable (an absolute path), or `None` when the node has none.
    pub git_bin: Option<PathBuf>,
    /// `<state dir>/marketplaces`.
    pub root: PathBuf,
    pub limits: Limits,
}

impl Fetcher {
    pub fn new(git_bin: Option<PathBuf>, root: PathBuf, limits: Limits) -> Self {
        Self {
            git_bin,
            root,
            limits,
        }
    }

    fn home(&self) -> Result<PathBuf, FetchError> {
        let home = self.root.join(".home");
        make_private_dir(&home)?;
        Ok(home)
    }

    fn tmp(&self) -> Result<PathBuf, FetchError> {
        let tmp = self.root.join(".tmp");
        make_private_dir(&tmp)?;
        Ok(tmp)
    }

    /// `<root>/<id>/repo.git`.
    pub fn cache_dir(&self, id: &str) -> Result<PathBuf, FetchError> {
        if !valid_id(id) {
            return Err(FetchError::Invalid(format!(
                "`{id}` is not a marketplace id"
            )));
        }
        Ok(self.root.join(id).join("repo.git"))
    }

    /// Removes the cache of a marketplace (a removed marketplace, or one grown past its cap).
    pub fn wipe(&self, id: &str) -> Result<(), FetchError> {
        if !valid_id(id) {
            return Err(FetchError::Invalid(format!(
                "`{id}` is not a marketplace id"
            )));
        }
        match std::fs::remove_dir_all(self.root.join(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(FetchError::Io(format!("cannot remove the cache: {e}"))),
        }
    }

    /// A lease on the guard proxy for one https call (revoked when dropped).
    fn proxy_for(&self, remote: &Remote) -> Result<Option<super::guard::ProxyLease>, FetchError> {
        match remote {
            Remote::Https { .. } => super::guard::shared()
                .map(|g| Some(g.lease()))
                .map_err(FetchError::Io),
            #[cfg(any(test, feature = "process-guest"))]
            Remote::File { .. } => Ok(None),
        }
    }

    pub(crate) fn scrub(&self, text: &str) -> String {
        let t = text
            .replace(&self.root.display().to_string(), "<state>")
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>();
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        t.chars().take(300).collect()
    }

    fn run(&self, call: Call<'_>) -> Result<Vec<u8>, FetchError> {
        let git = self.git_bin.clone().ok_or(FetchError::GitMissing)?;
        let (home, tmp) = (self.home()?, self.tmp()?);
        let proxy = match call.remote {
            Some(r) => self.proxy_for(r)?,
            None => None,
        };
        let mut cmd = Command::new(&git);
        cmd.current_dir(&home)
            .env_clear()
            .envs(child_env(&git, &home, &tmp, call.remote))
            .args(config_args(call.remote, proxy.as_ref().map(|l| l.url())));
        if let Some(d) = call.git_dir {
            cmd.arg("--git-dir").arg(d);
        }
        cmd.args(&call.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FetchError::GitMissing
            } else {
                FetchError::Io(format!("cannot start git: {e}"))
            }
        })?;
        let pid = child.id();
        let overflow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let read_capped =
            |mut pipe: Box<dyn Read + Send>,
             cap: usize,
             flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>| {
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 16 * 1024];
                    loop {
                        match pipe.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if buf.len() + n > cap {
                                    if let Some(f) = &flag {
                                        f.store(true, std::sync::atomic::Ordering::SeqCst);
                                    }
                                    buf.extend_from_slice(&chunk[..cap.saturating_sub(buf.len())]);
                                    break;
                                }
                                buf.extend_from_slice(&chunk[..n]);
                            }
                        }
                    }
                    buf
                })
            };
        let out_pipe = child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>);
        let err_pipe = child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>);
        let out_t = out_pipe.map(|p| read_capped(p, call.max_stdout, Some(overflow.clone())));
        let err_t = err_pipe.map(|p| read_capped(p, 16 * 1024, None));
        let started = Instant::now();
        let mut last_size_check = Instant::now();
        let mut verdict: Option<FetchError> = None;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) => {}
                Err(e) => {
                    verdict = Some(FetchError::Io(format!("git: {e}")));
                    break None;
                }
            }
            if started.elapsed() > call.timeout {
                verdict = Some(FetchError::Timeout(call.what));
                break None;
            }
            if overflow.load(std::sync::atomic::Ordering::SeqCst) {
                verdict = Some(FetchError::TooLarge(format!(
                    "git {} printed more than {} bytes",
                    call.what, call.max_stdout
                )));
                break None;
            }
            if let Some((dir, cap)) = call.watch {
                if last_size_check.elapsed() > Duration::from_millis(200) {
                    last_size_check = Instant::now();
                    if dir_size(dir) > cap {
                        verdict = Some(FetchError::TooLarge(format!(
                            "the repository is larger than {} MiB",
                            cap / (1024 * 1024)
                        )));
                        break None;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(15));
        };
        if status.is_none() {
            kill_group(pid);
            let _ = child.kill();
            let _ = child.wait();
        } else {
            // The leader exited; make sure no helper it started outlives it.
            kill_group(pid);
        }
        let stdout = out_t
            .map(|t| t.join().unwrap_or_default())
            .unwrap_or_default();
        let stderr = err_t
            .map(|t| t.join().unwrap_or_default())
            .unwrap_or_default();
        if verdict.is_none() && overflow.load(std::sync::atomic::Ordering::SeqCst) {
            // The pipe was closed on a process that kept talking; it may have died of SIGPIPE first.
            verdict = Some(FetchError::TooLarge(format!(
                "git {} printed more than {} bytes",
                call.what, call.max_stdout
            )));
        }
        if let Some(v) = verdict {
            return Err(v);
        }
        match status {
            Some(s) if s.success() => Ok(stdout),
            _ => Err(FetchError::Git(
                self.scrub(&String::from_utf8_lossy(&stderr))
                    .replace(proxy.as_ref().map_or("\u{0}", |l| l.token()), "***"),
            )),
        }
    }

    // ---- reads of a remote -----------------------------------------------------------------

    /// Refuses a remote whose host does not resolve to public addresses (https only). The guard
    /// proxy checks again when Git connects.
    pub fn precheck(&self, remote: &Remote) -> Result<(), FetchError> {
        match remote {
            Remote::Https { host, .. } => Ok(check_host_public_system(host)?),
            #[cfg(any(test, feature = "process-guest"))]
            Remote::File { .. } => Ok(()),
        }
    }

    /// `git ls-remote`: the default branch head and the tags (peeled to commits).
    pub fn ls_remote(&self, remote: &Remote) -> Result<RemoteRefs, FetchError> {
        let out = self.run(Call {
            git_dir: None,
            remote: Some(remote),
            args: vec![
                "ls-remote".into(),
                "--".into(),
                remote.url().into(),
                "HEAD".into(),
                "refs/tags/*".into(),
            ],
            timeout: self.limits.ls_remote,
            what: "ls-remote",
            max_stdout: self.limits.max_ls_bytes,
            watch: None,
        })?;
        parse_ls_remote(&String::from_utf8_lossy(&out), self.limits.max_tags)
    }

    /// Makes the bare cache of `id` if it is not there yet.
    pub fn ensure_repo(&self, id: &str) -> Result<PathBuf, FetchError> {
        let dir = self.cache_dir(id)?;
        if dir.join("HEAD").is_file() {
            return Ok(dir);
        }
        if let Some(parent) = dir.parent() {
            make_private_dir(parent)?;
        }
        self.run(Call {
            git_dir: None,
            remote: None,
            args: vec![
                "init".into(),
                "--bare".into(),
                "-q".into(),
                "--template=".into(),
                "--initial-branch=main".into(),
                dir.clone().into_os_string(),
            ],
            timeout: self.limits.local,
            what: "init",
            max_stdout: 4096,
            watch: None,
        })?;
        Ok(dir)
    }

    /// Fetches the remote's `HEAD` into `refs/vam/head` and each tag into its own ref, shallow
    /// (depth 1), with no other tags and no submodules. Tags are validated first.
    pub fn fetch(&self, id: &str, remote: &Remote, tags: &[String]) -> Result<(), FetchError> {
        for t in tags {
            check_git_ref(t).map_err(|why| FetchError::Invalid(format!("tag `{t}` {why}")))?;
        }
        let dir = self.ensure_repo(id)?;
        let mut args: Vec<OsString> = vec![
            "fetch".into(),
            "--no-tags".into(),
            "--no-recurse-submodules".into(),
            "--depth=1".into(),
            "--force".into(),
            "--".into(),
            remote.url().into(),
            format!("+HEAD:{HEAD_REF}").into(),
        ];
        for t in tags {
            args.push(format!("+refs/tags/{t}:{}", tag_ref(t)).into());
        }
        self.run(Call {
            git_dir: Some(&dir),
            remote: Some(remote),
            args,
            timeout: self.limits.fetch,
            what: "fetch",
            max_stdout: 64 * 1024,
            watch: Some((&dir, self.limits.max_cache_bytes)),
        })?;
        Ok(())
    }

    // ---- reads of the cache (never the network) --------------------------------------------

    fn local(
        &self,
        id: &str,
        args: Vec<OsString>,
        what: &'static str,
        max_stdout: usize,
    ) -> Result<Vec<u8>, FetchError> {
        let dir = self.cache_dir(id)?;
        self.run(Call {
            git_dir: Some(&dir),
            remote: None,
            args,
            timeout: self.limits.local,
            what,
            max_stdout,
            watch: None,
        })
    }

    /// The commit a cached ref points at (peeled), or `None` when the ref is not in the cache.
    pub fn ref_commit(&self, id: &str, refname: &str) -> Result<Option<String>, FetchError> {
        if !refname.starts_with("refs/vam/") || refname.contains("..") {
            return Err(FetchError::Invalid("not a cache ref".into()));
        }
        match self.local(
            id,
            vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                format!("{refname}^{{commit}}").into(),
            ],
            "rev-parse",
            256,
        ) {
            Ok(out) => {
                let s = String::from_utf8_lossy(&out).trim().to_string();
                Ok(valid_oid(&s).then_some(s))
            }
            // `--quiet` makes a missing ref exit 1 with no message.
            Err(FetchError::Git(m)) if m.is_empty() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The commit a fetched tag points at, from the cache.
    pub fn tag_commit(&self, id: &str, tag: &str) -> Result<Option<String>, FetchError> {
        check_git_ref(tag).map_err(|why| FetchError::Invalid(format!("tag `{tag}` {why}")))?;
        self.ref_commit(id, &tag_ref(tag))
    }

    /// The files under `dir` at `commit` (recursive). `dir` is a relative path inside the repo.
    pub fn list_tree(
        &self,
        id: &str,
        commit: &str,
        dir: &str,
    ) -> Result<Vec<TreeEntry>, FetchError> {
        if !valid_oid(commit) {
            return Err(FetchError::Invalid("not a commit id".into()));
        }
        vam_spec::marketplace::check_relative_path(dir)
            .map_err(|why| FetchError::Invalid(format!("path `{dir}` {why}")))?;
        let out = self.local(
            id,
            vec![
                "ls-tree".into(),
                "-r".into(),
                "-z".into(),
                "--long".into(),
                commit.into(),
                "--".into(),
                format!("{dir}/").into(),
            ],
            "ls-tree",
            4 * 1024 * 1024,
        )?;
        parse_ls_tree(&out)
    }

    /// Reads one regular file at `commit` (at most `max` bytes). A symlink, a submodule, a
    /// directory or a missing path is refused.
    pub fn read_file(
        &self,
        id: &str,
        commit: &str,
        path: &str,
        max: u64,
    ) -> Result<Vec<u8>, FetchError> {
        if !valid_oid(commit) {
            return Err(FetchError::Invalid("not a commit id".into()));
        }
        vam_spec::marketplace::check_relative_path(path)
            .map_err(|why| FetchError::Invalid(format!("path `{path}` {why}")))?;
        let out = self.local(
            id,
            vec![
                "ls-tree".into(),
                "-z".into(),
                "--long".into(),
                commit.into(),
                "--".into(),
                path.into(),
            ],
            "ls-tree",
            64 * 1024,
        )?;
        let mut entries = parse_ls_tree(&out)?;
        let entry = match (entries.pop(), entries.is_empty()) {
            (Some(e), true) if e.path == path => e,
            _ => {
                return Err(FetchError::Invalid(format!(
                    "`{path}` is not in the repository"
                )))
            }
        };
        check_entry(&entry)?;
        if entry.size > max {
            return Err(FetchError::TooLarge(format!(
                "`{path}` is larger than {max} bytes"
            )));
        }
        self.blob(id, &entry)
    }

    fn blob(&self, id: &str, entry: &TreeEntry) -> Result<Vec<u8>, FetchError> {
        let data = self.local(
            id,
            vec!["cat-file".into(), "blob".into(), entry.oid.clone().into()],
            "cat-file",
            entry.size as usize + 1,
        )?;
        if data.len() as u64 != entry.size {
            return Err(FetchError::Invalid(format!(
                "`{}` did not read back at its size",
                entry.path
            )));
        }
        Ok(data)
    }

    /// Writes the directory `dir` of `commit` into the existing, empty directory `dest`. Only
    /// regular files are written; a symlink, a submodule, a `.git` component or a path that would
    /// leave `dest` fails the whole export and nothing is read outside the tree.
    pub fn export_dir(
        &self,
        id: &str,
        commit: &str,
        dir: &str,
        dest: &Path,
    ) -> Result<(), FetchError> {
        let entries = self.list_tree(id, commit, dir)?;
        if entries.is_empty() {
            return Err(FetchError::Invalid(format!(
                "`{dir}` is not a directory at this commit"
            )));
        }
        if entries.len() > self.limits.max_export_files {
            return Err(FetchError::TooLarge(format!(
                "`{dir}` has more than {} files",
                self.limits.max_export_files
            )));
        }
        let mut total = 0u64;
        for e in &entries {
            check_entry(e)?;
            if e.size > MAX_PACKAGE_FILE_BYTES {
                return Err(FetchError::TooLarge(format!(
                    "`{}` is larger than {MAX_PACKAGE_FILE_BYTES} bytes",
                    e.path
                )));
            }
            total += e.size;
        }
        if total > self.limits.max_export_bytes {
            return Err(FetchError::TooLarge(format!(
                "`{dir}` is larger than {} bytes",
                self.limits.max_export_bytes
            )));
        }
        let prefix = format!("{dir}/");
        for e in &entries {
            let rel = e
                .path
                .strip_prefix(&prefix)
                .ok_or_else(|| FetchError::Invalid(format!("`{}` is outside `{dir}`", e.path)))?;
            let target = safe_join(dest, rel)?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| FetchError::Io(format!("cannot write the export: {e}")))?;
            }
            let data = self.blob(id, e)?;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|e| FetchError::Io(format!("cannot write `{rel}`: {e}")))?;
            std::io::Write::write_all(&mut f, &data)
                .map_err(|e| FetchError::Io(format!("cannot write `{rel}`: {e}")))?;
            let mode = if e.mode == "100755" { 0o755 } else { 0o644 };
            let _ = f.set_permissions(std::fs::Permissions::from_mode(mode));
        }
        Ok(())
    }

    /// A fresh scratch directory inside the marketplace's cache folder (removed on drop).
    pub fn scratch(&self, id: &str) -> Result<tempfile::TempDir, FetchError> {
        if !valid_id(id) {
            return Err(FetchError::Invalid(format!(
                "`{id}` is not a marketplace id"
            )));
        }
        let base = self.root.join(id).join("scratch");
        make_private_dir(&base)?;
        tempfile::Builder::new()
            .prefix("export-")
            .tempdir_in(&base)
            .map_err(|e| FetchError::Io(format!("cannot make a scratch directory: {e}")))
    }
}

fn make_private_dir(p: &Path) -> Result<(), FetchError> {
    std::fs::create_dir_all(p)
        .map_err(|e| FetchError::Io(format!("cannot create {}: {e}", p.display())))?;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

fn check_entry(e: &TreeEntry) -> Result<(), FetchError> {
    match (e.kind.as_str(), e.mode.as_str()) {
        ("blob", "100644" | "100755") => Ok(()),
        ("blob", "120000") => Err(FetchError::Invalid(format!(
            "`{}` is a symlink, which a marketplace may not contain",
            e.path
        ))),
        ("commit", _) => Err(FetchError::Invalid(format!(
            "`{}` is a submodule, which a marketplace may not contain",
            e.path
        ))),
        _ => Err(FetchError::Invalid(format!(
            "`{}` is not a regular file",
            e.path
        ))),
    }
}

/// `dest/rel` for a relative path whose components are all plain names (not `..`, not `.git`).
fn safe_join(dest: &Path, rel: &str) -> Result<PathBuf, FetchError> {
    let mut out = dest.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(seg) => {
                let s = seg.to_string_lossy();
                if s.eq_ignore_ascii_case(".git") || s.contains('\\') || s.contains('\0') {
                    return Err(FetchError::Invalid(format!("`{rel}` has a forbidden name")));
                }
                out.push(seg);
            }
            _ => {
                return Err(FetchError::Invalid(format!(
                    "`{rel}` is not a plain relative path"
                )))
            }
        }
    }
    if out == dest {
        return Err(FetchError::Invalid("empty path".into()));
    }
    Ok(out)
}

/// Parses `git ls-remote` output: `<sha>\t<ref>` lines.
pub(crate) fn parse_ls_remote(out: &str, max_tags: usize) -> Result<RemoteRefs, FetchError> {
    let mut head: Option<String> = None;
    let mut direct: BTreeMap<String, String> = BTreeMap::new();
    let mut peeled: BTreeMap<String, String> = BTreeMap::new();
    for line in out.lines() {
        let Some((sha, name)) = line.split_once('\t') else {
            continue;
        };
        if !valid_oid(sha) {
            return Err(FetchError::Invalid(
                "the repository did not answer with SHA-1 object ids".into(),
            ));
        }
        if name == "HEAD" {
            head = Some(sha.to_string());
        } else if let Some(tag) = name.strip_prefix("refs/tags/") {
            let (tag, is_peeled) = match tag.strip_suffix("^{}") {
                Some(t) => (t, true),
                None => (tag, false),
            };
            if check_git_ref(tag).is_err() {
                continue;
            }
            let map = if is_peeled { &mut peeled } else { &mut direct };
            map.insert(tag.to_string(), sha.to_string());
            if direct.len().max(peeled.len()) > max_tags {
                return Err(FetchError::TooLarge(format!(
                    "the repository has more than {max_tags} tags"
                )));
            }
        }
    }
    let head = head.ok_or_else(|| {
        FetchError::Invalid("the repository has no default branch (is it empty?)".into())
    })?;
    let mut tags = direct;
    for (t, c) in peeled {
        tags.insert(t, c);
    }
    Ok(RemoteRefs { head, tags })
}

/// Parses `git ls-tree -z --long` output: `<mode> <type> <oid> <size>\t<path>\0`.
pub(crate) fn parse_ls_tree(out: &[u8]) -> Result<Vec<TreeEntry>, FetchError> {
    let mut entries = Vec::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let text = std::str::from_utf8(rec).map_err(|_| {
            FetchError::Invalid("a file name in the repository is not UTF-8".into())
        })?;
        let (meta, path) = text
            .split_once('\t')
            .ok_or_else(|| FetchError::Invalid("unreadable tree listing".into()))?;
        let mut f = meta.split_whitespace();
        let (mode, kind, oid, size) = (f.next(), f.next(), f.next(), f.next());
        let (Some(mode), Some(kind), Some(oid), Some(size)) = (mode, kind, oid, size) else {
            return Err(FetchError::Invalid("unreadable tree listing".into()));
        };
        if !valid_oid(oid) {
            return Err(FetchError::Invalid("unreadable tree listing".into()));
        }
        entries.push(TreeEntry {
            mode: mode.to_string(),
            kind: kind.to_string(),
            oid: oid.to_string(),
            // A submodule shows `-` for its size.
            size: size.parse().unwrap_or(0),
            path: path.to_string(),
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests;
