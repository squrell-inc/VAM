//! `vam upgrade`: replace this `vam` with a newer (or pinned) release after checking it.
//! Linux and macOS only (Windows comes in v0.2.0). Design: `design/release-artifacts.md` §5.7.
//!
//! Every check runs before the running binary is touched: the version parses, the build target
//! has a release asset, `SHA256SUMS` lists the archive's hash and the download matches it, `gh`
//! (when on the `PATH`) verifies the attestation, and the extracted binary starts and reports the
//! release's version. Only then is it renamed over the old file. Any failure before that leaves
//! the old binary as it was. `vamd` is never touched, and nothing runs unless the user runs
//! `vam upgrade`: no other command calls the network on its own.

use anyhow::Result;
use clap::Args;

/// The repository releases come from. The one place the slug is written.
const REPO: &str = "squrell-inc/VAM";
/// The workflow whose publish job attests the assets (the `--signer-workflow` of the check).
const RELEASE_WORKFLOW: &str = ".github/workflows/release.yml";

#[derive(Args)]
pub struct UpgradeArgs {
    /// Only report what an upgrade would do; change nothing.
    #[arg(long)]
    pub check: bool,
    /// Install this release (for example v0.1.0-rc.2), newer or older than the current one.
    #[arg(long, value_name = "VERSION", conflicts_with = "pre")]
    pub version: Option<String>,
    /// Also consider pre-releases (rc) when choosing the latest release.
    #[arg(long)]
    pub pre: bool,
}

#[cfg(unix)]
pub async fn run(args: UpgradeArgs) -> Result<()> {
    imp::run(args).await
}

#[cfg(not(unix))]
pub async fn run(_args: UpgradeArgs) -> Result<()> {
    anyhow::bail!("vam upgrade is not supported on this OS yet (Windows comes in v0.2.0)")
}

#[cfg(unix)]
mod imp {
    use std::cmp::Ordering;
    use std::ffi::{OsStr, OsString};
    use std::fmt;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::{Component, Path, PathBuf};
    use std::time::Duration;

    use anyhow::{anyhow, bail, Context, Result};
    use serde::Deserialize;
    use sha2::{Digest, Sha256};

    use super::{UpgradeArgs, RELEASE_WORKFLOW, REPO};
    use crate::output::{fail, ExitCode};

    const API_BASE: &str = "https://api.github.com";
    /// The release asset targets (design §5.1). Windows comes in v0.2.0.
    const SHIPPED_TARGETS: [&str; 4] = [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
    ];
    const MAX_API: u64 = 4 << 20;
    const MAX_SUMS: u64 = 1 << 20;
    const MAX_ARCHIVE: u64 = 200 << 20;
    const MAX_BINARY: u64 = 256 << 20;
    const SMOKE_LIMIT: Duration = Duration::from_secs(20);
    const ATTEST_LIMIT: Duration = Duration::from_secs(120);

    // ---- versions ------------------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Ident {
        Num(u64),
        Alpha(String),
    }

    impl Ord for Ident {
        fn cmp(&self, other: &Self) -> Ordering {
            match (self, other) {
                (Ident::Num(a), Ident::Num(b)) => a.cmp(b),
                (Ident::Num(_), Ident::Alpha(_)) => Ordering::Less,
                (Ident::Alpha(_), Ident::Num(_)) => Ordering::Greater,
                (Ident::Alpha(a), Ident::Alpha(b)) => a.cmp(b),
            }
        }
    }
    impl PartialOrd for Ident {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    /// `major.minor.patch[-pre]`, plus `dev` for a dev build (`+dev` / `+dev.N`), which counts as
    /// newer than its base tag and older than the next release.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Version {
        major: u64,
        minor: u64,
        patch: u64,
        pre: Vec<Ident>,
        dev: bool,
    }

    fn number(s: &str, what: &str) -> Result<u64> {
        if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
            bail!("{what} '{s}' is not a number");
        }
        if s.len() > 1 && s.starts_with('0') {
            bail!("{what} '{s}' has a leading zero");
        }
        s.parse().map_err(|_| anyhow!("{what} '{s}' is too large"))
    }

    impl Version {
        /// Parses `v0.1.0`, `0.1.0-rc.2`; with `allow_dev` also `0.3.0+dev.14`. Other build
        /// metadata is refused.
        fn parse(text: &str, allow_dev: bool) -> Result<Self> {
            let bad = |why: String| anyhow!("'{text}' is not a release version: {why}");
            let t = text.strip_prefix('v').unwrap_or(text);
            let (rest, build) = match t.split_once('+') {
                Some((r, b)) => (r, Some(b)),
                None => (t, None),
            };
            let dev = match build {
                None => false,
                Some(b)
                    if allow_dev
                        && (b == "dev"
                            || b.strip_prefix("dev.").is_some_and(|n| {
                                !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())
                            })) =>
                {
                    true
                }
                Some(_) => return Err(bad("build metadata is not accepted".into())),
            };
            let (core, pre) = match rest.split_once('-') {
                Some((c, p)) => (c, Some(p)),
                None => (rest, None),
            };
            let parts: Vec<&str> = core.split('.').collect();
            if parts.len() != 3 {
                return Err(bad("expected major.minor.patch".into()));
            }
            let n = |s: &str, w: &str| number(s, w).map_err(|e| bad(e.to_string()));
            let (major, minor, patch) = (
                n(parts[0], "major")?,
                n(parts[1], "minor")?,
                n(parts[2], "patch")?,
            );
            let mut idents = Vec::new();
            if let Some(p) = pre {
                for id in p.split('.') {
                    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                    {
                        return Err(bad(format!("bad pre-release identifier '{id}'")));
                    }
                    idents.push(if id.bytes().all(|c| c.is_ascii_digit()) {
                        Ident::Num(n(id, "pre-release number")?)
                    } else {
                        Ident::Alpha(id.to_string())
                    });
                }
            }
            Ok(Self {
                major,
                minor,
                patch,
                pre: idents,
                dev,
            })
        }

        fn release(text: &str) -> Result<Self> {
            Self::parse(text, false)
        }

        fn tag(&self) -> String {
            format!("v{self}")
        }
    }

    impl fmt::Display for Version {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
            if !self.pre.is_empty() {
                let ids: Vec<String> = self
                    .pre
                    .iter()
                    .map(|i| match i {
                        Ident::Num(n) => n.to_string(),
                        Ident::Alpha(a) => a.clone(),
                    })
                    .collect();
                write!(f, "-{}", ids.join("."))?;
            }
            if self.dev {
                write!(f, "+dev")?;
            }
            Ok(())
        }
    }

    impl Ord for Version {
        fn cmp(&self, other: &Self) -> Ordering {
            (self.major, self.minor, self.patch)
                .cmp(&(other.major, other.minor, other.patch))
                .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => self.pre.cmp(&other.pre),
                })
                .then(self.dev.cmp(&other.dev))
        }
    }
    impl PartialOrd for Version {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    // ---- assets and hashes ---------------------------------------------------------------

    fn asset_triple(target: &str) -> Result<&'static str> {
        SHIPPED_TARGETS
            .iter()
            .copied()
            .find(|t| *t == target)
            .ok_or_else(|| {
                anyhow!(
                    "no release asset for this build target ({target}); releases ship vam for {}",
                    SHIPPED_TARGETS.join(", ")
                )
            })
    }

    /// The archive `package.sh` makes: the version without the tag's `v`.
    fn asset_name(v: &Version, triple: &str) -> String {
        format!("vam-{v}-{triple}.tar.gz")
    }

    /// The hash `SHA256SUMS` lists for `name` (`<hash>  <name>` or `<hash> *<name>`).
    fn listed_hash(sums: &str, name: &str) -> Option<String> {
        sums.lines().find_map(|l| {
            let mut it = l.split_whitespace();
            let (hash, file) = (it.next()?, it.next()?);
            (it.next().is_none() && file.trim_start_matches('*') == name)
                .then(|| hash.to_ascii_lowercase())
        })
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    // ---- the GitHub API ------------------------------------------------------------------

    #[derive(Debug, Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }

    #[derive(Debug, Deserialize)]
    struct Release {
        tag_name: String,
        #[serde(default)]
        prerelease: bool,
        #[serde(default)]
        draft: bool,
        #[serde(default)]
        assets: Vec<Asset>,
    }

    impl Release {
        fn asset(&self, name: &str) -> Result<&Asset> {
            self.assets
                .iter()
                .find(|a| a.name == name)
                .ok_or_else(|| anyhow!("release {} has no asset named {name}", self.tag_name))
        }
    }

    /// The API base. Only a build with the `upgrade-seam` feature (the e2e `vam`) honours an
    /// override, and only for a loopback URL.
    #[cfg(feature = "upgrade-seam")]
    fn api_base() -> Result<String> {
        match std::env::var("VAM_UPGRADE_API_URL") {
            Ok(u) if !u.is_empty() => {
                if !is_loopback_url(&u) {
                    bail!("the release API override must be an http://127.0.0.1, localhost or [::1] URL");
                }
                Ok(u.trim_end_matches('/').to_string())
            }
            _ => Ok(API_BASE.to_string()),
        }
    }

    #[cfg(not(feature = "upgrade-seam"))]
    fn api_base() -> Result<String> {
        Ok(API_BASE.to_string())
    }

    #[cfg_attr(not(feature = "upgrade-seam"), allow(dead_code))]
    fn is_loopback_url(u: &str) -> bool {
        reqwest::Url::parse(u).is_ok_and(|u| {
            u.scheme() == "http"
                && matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
        })
    }

    fn net_error(url: &str, e: reqwest::Error) -> anyhow::Error {
        let msg = format!("cannot reach {url}: {e}");
        if e.is_connect() || e.is_timeout() {
            fail(ExitCode::Unreachable, msg)
        } else {
            // Not a reqwest error in the chain: main would probe the node's host to explain it.
            anyhow!(msg)
        }
    }

    async fn read_capped(mut resp: reqwest::Response, cap: u64, what: &str) -> Result<Vec<u8>> {
        if resp.content_length().is_some_and(|n| n > cap) {
            bail!("{what} is larger than {cap} bytes; refusing it");
        }
        let mut buf = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| anyhow!("reading {what}: {e}"))?
        {
            if buf.len() as u64 + chunk.len() as u64 > cap {
                bail!("{what} is larger than {cap} bytes; refusing it");
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }

    struct Api {
        client: reqwest::Client,
        base: String,
        token: Option<String>,
    }

    impl Api {
        fn new(base: &str, token: Option<String>, user_agent: &str) -> Result<Self> {
            // The bundled roots only: `VAM_CA_FILE` and `vam config set-ca` name the node's CA,
            // which has nothing to do with GitHub, and a stale one must not block an upgrade.
            let client = crate::transport::client_builder_with(None)?
                .user_agent(user_agent)
                .https_only(base.starts_with("https://"))
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(300))
                .build()
                .map_err(|e| anyhow!("cannot build the HTTP client: {e}"))?;
            Ok(Self {
                client,
                base: base.to_string(),
                token,
            })
        }

        /// A token goes only to GitHub's own API host, never to an override or an asset URL.
        fn authorization(&self) -> Option<String> {
            (self.base == API_BASE)
                .then(|| self.token.as_ref().map(|t| format!("Bearer {t}")))
                .flatten()
        }

        /// `None` on 404.
        async fn api_get(&self, path: &str) -> Result<Option<Vec<u8>>> {
            let url = format!("{}{path}", self.base);
            let mut rq = self
                .client
                .get(&url)
                .header("Accept", "application/vnd.github+json");
            if let Some(a) = self.authorization() {
                rq = rq.header("Authorization", a);
            }
            let resp = rq.send().await.map_err(|e| net_error(&url, e))?;
            let status = resp.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if !status.is_success() {
                let hint = if status.as_u16() == 403 || status.as_u16() == 429 {
                    " (rate limited? set GH_TOKEN or GITHUB_TOKEN)"
                } else {
                    ""
                };
                bail!("the GitHub API answered {status} for {url}{hint}");
            }
            read_capped(resp, MAX_API, "the GitHub API answer")
                .await
                .map(Some)
        }

        async fn download(&self, url: &str, cap: u64, what: &str) -> Result<Vec<u8>> {
            self.check_download_url(url)?;
            let resp = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|e| net_error(url, e))?;
            if !resp.status().is_success() {
                bail!("downloading {what}: {url} answered {}", resp.status());
            }
            read_capped(resp, cap, what).await
        }

        /// Asset URLs must be https on github.com, or on the API's own host.
        fn check_download_url(&self, url: &str) -> Result<()> {
            let u = reqwest::Url::parse(url).map_err(|e| anyhow!("bad download URL {url}: {e}"))?;
            let base = reqwest::Url::parse(&self.base)
                .map_err(|e| anyhow!("bad API URL {}: {e}", self.base))?;
            let on_github = u.scheme() == "https" && u.host_str() == Some("github.com");
            let on_api = u.scheme() == base.scheme()
                && u.host_str() == base.host_str()
                && u.port_or_known_default() == base.port_or_known_default();
            if on_github || on_api {
                Ok(())
            } else {
                bail!("refusing to download from {url}: not github.com or the release API host")
            }
        }
    }

    enum Selection {
        Final,
        Pre,
        Pin(Version),
    }

    fn parse_release(body: &[u8]) -> Result<Release> {
        serde_json::from_slice(body).map_err(|e| anyhow!("unreadable release from the API: {e}"))
    }

    async fn resolve(api: &Api, sel: &Selection) -> Result<(Version, Release)> {
        let releases = format!("/repos/{REPO}/releases");
        match sel {
            Selection::Pin(v) => {
                let Some(body) = api.api_get(&format!("{releases}/tags/{}", v.tag())).await? else {
                    bail!("{REPO} has no published release {}", v.tag());
                };
                let r = parse_release(&body)?;
                if r.draft {
                    bail!("release {} is a draft, not published", r.tag_name);
                }
                Ok((v.clone(), r))
            }
            Selection::Final => {
                let Some(body) = api.api_get(&format!("{releases}/latest")).await? else {
                    bail!(
                        "no final release yet; pass --version v0.1.0-rc.N to install a given \
                         release, or --pre to consider release candidates"
                    );
                };
                let r = parse_release(&body)?;
                let v = Version::release(&r.tag_name)
                    .with_context(|| format!("the latest release is tagged {}", r.tag_name))?;
                Ok((v, r))
            }
            Selection::Pre => {
                let Some(body) = api.api_get(&format!("{releases}?per_page=100")).await? else {
                    bail!("{REPO} has no releases");
                };
                let list: Vec<Release> = serde_json::from_slice(&body)
                    .map_err(|e| anyhow!("unreadable release list from the API: {e}"))?;
                list.into_iter()
                    .filter(|r| !r.draft)
                    .filter_map(|r| Version::release(&r.tag_name).ok().map(|v| (v, r)))
                    .max_by(|a, b| a.0.cmp(&b.0))
                    .ok_or_else(|| anyhow!("{REPO} has no published release yet"))
            }
        }
    }

    // ---- checks on the machine ------------------------------------------------------------

    fn in_node_bundle(exe: &Path) -> bool {
        exe.parent().is_some_and(|d| d.join("vamd").is_file())
    }

    fn probe_writable(dir: &Path) -> Result<()> {
        let probe = dir.join(format!(".vam-upgrade-probe.{}", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(_) => {
                let _ = fs::remove_file(&probe);
                Ok(())
            }
            Err(e) => bail!(
                "cannot write to {} ({e}); re-run with sudo, or `vam dock` a copy into a \
                 directory you own and upgrade that one",
                dir.display()
            ),
        }
    }

    fn find_in_path(name: &str, path: &OsStr) -> Option<PathBuf> {
        std::env::split_paths(path)
            .filter(|d| !d.as_os_str().is_empty())
            .map(|d| d.join(name))
            .find(|p| {
                p.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    }

    /// Runs a program and captures its output, retrying a file that is briefly busy because it
    /// was just written.
    async fn run_capture(
        prog: &Path,
        args: &[&OsStr],
        limit: Duration,
    ) -> Result<std::process::Output> {
        let mut last = None;
        for _ in 0..5 {
            let mut c = tokio::process::Command::new(prog);
            c.args(args)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            match tokio::time::timeout(limit, c.output()).await {
                Err(_) => bail!("{} did not finish in {}s", prog.display(), limit.as_secs()),
                Ok(Ok(o)) => return Ok(o),
                Ok(Err(e)) if e.kind() == io::ErrorKind::ExecutableFileBusy => {
                    last = Some(e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Ok(Err(e)) => return Err(anyhow!("cannot run {}: {e}", prog.display())),
            }
        }
        Err(anyhow!(
            "cannot run {}: {}",
            prog.display(),
            last.map(|e| e.to_string()).unwrap_or_default()
        ))
    }

    fn attest_command() -> Vec<String> {
        vec![
            "attestation".into(),
            "verify".into(),
            "<archive>".into(),
            "-R".into(),
            REPO.into(),
            "--signer-workflow".into(),
            format!("{REPO}/{RELEASE_WORKFLOW}"),
        ]
    }

    async fn verify_attestation(gh: &Path, archive: &Path) -> Result<()> {
        let mut args: Vec<OsString> = attest_command().into_iter().map(Into::into).collect();
        args[2] = archive.as_os_str().to_owned();
        let args: Vec<&OsStr> = args.iter().map(|a| a.as_os_str()).collect();
        let out = run_capture(gh, &args, ATTEST_LIMIT).await?;
        if out.status.success() {
            return Ok(());
        }
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Err(fail(ExitCode::VerifyFailed, attest_refusal(&text)))
    }

    /// The oldest gh whose `gh attestation verify` takes `--signer-workflow` (`gh attestation`
    /// itself came in 2.49.0, the flag in 2.51.0).
    const MIN_GH: &str = "2.51.0";

    /// Why `gh attestation verify` failed, in words: a gh too old to run the check is named as
    /// such, anything else is a refusal. Nothing is replaced either way.
    fn attest_refusal(gh_output: &str) -> String {
        let text = gh_output.trim();
        let too_old = text.contains("unknown command \"attestation\"")
            || text.contains("unknown flag: --signer-workflow");
        if too_old {
            format!(
                "this gh is too old to check the attestation: `gh attestation verify \
                 --signer-workflow` needs gh {MIN_GH} or newer. Nothing was replaced. Upgrade gh \
                 (`gh --version` shows yours) and run `vam upgrade` again.\n{text}"
            )
        } else {
            format!(
                "`gh attestation verify` refused the download; nothing was replaced. If gh is \
                 not signed in, run `gh auth login`.\n{text}"
            )
        }
    }

    // ---- extract, smoke-run, replace -------------------------------------------------------

    /// Writes the archive's `vam` (at the top or one directory down) to `dest`.
    fn extract_vam(archive: &[u8], dest: &mut fs::File) -> Result<()> {
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
        for entry in ar.entries().context("read the archive")? {
            let entry = entry.context("read an archive entry")?;
            let path = entry.path().context("read an archive path")?.into_owned();
            let mut names = Vec::new();
            let mut plain = true;
            for c in path.components() {
                match c {
                    Component::Normal(n) => names.push(n.to_owned()),
                    Component::CurDir => {}
                    _ => plain = false,
                }
            }
            let named_vam = names.last().is_some_and(|n| n == "vam");
            if !plain || names.len() > 2 || !named_vam {
                continue;
            }
            let kind = entry.header().entry_type();
            if kind.is_dir() {
                continue;
            }
            if !kind.is_file() {
                bail!("the archive's `vam` is not a regular file");
            }
            let mut body = entry.take(MAX_BINARY + 1);
            let n = io::copy(&mut body, dest).context("write the new vam")?;
            if n == 0 {
                bail!("the archive's `vam` is empty");
            }
            if n > MAX_BINARY {
                bail!("the archive's `vam` is larger than {MAX_BINARY} bytes; refusing it");
            }
            return Ok(());
        }
        bail!("the archive holds no `vam` file")
    }

    /// The new binary beside the old one; removed on drop unless it was renamed away.
    struct Staged(PathBuf);

    impl Drop for Staged {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn stage(exe: &Path, archive: &[u8]) -> Result<Staged> {
        let dir = exe
            .parent()
            .ok_or_else(|| anyhow!("{} has no directory", exe.display()))?;
        let name = exe.file_name().unwrap_or(OsStr::new("vam"));
        let tmp = dir.join(format!(
            ".{}.new.{}",
            name.to_string_lossy(),
            std::process::id()
        ));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        let staged = Staged(tmp);
        extract_vam(archive, &mut f)?;
        let mode = fs::metadata(exe)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o755);
        f.set_permissions(fs::Permissions::from_mode(if mode & 0o111 == 0 {
            0o755
        } else {
            mode
        }))?;
        f.sync_all().context("flush the new vam to disk")?;
        Ok(staged)
    }

    /// The new file must start and name the release it is meant to be.
    async fn smoke(path: &Path, want: &Version) -> Result<String> {
        let out = run_capture(path, &[OsStr::new("--version")], SMOKE_LIMIT)
            .await
            .context("the new vam does not start")?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().next().unwrap_or_default().trim().to_string();
        let prefix = format!("vam {want}");
        let named = line
            .strip_prefix(&prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace));
        if !out.status.success() || !named {
            bail!(
                "the new vam does not report {prefix} (exit {:?}, printed '{line}'); \
                 nothing was replaced",
                out.status.code()
            );
        }
        Ok(line)
    }

    fn replace(staged: &Staged, exe: &Path) -> Result<()> {
        fs::rename(&staged.0, exe).with_context(|| format!("replace {}", exe.display()))?;
        if let Some(dir) = exe.parent() {
            let _ = fs::File::open(dir).and_then(|d| d.sync_all());
        }
        Ok(())
    }

    // ---- the command -----------------------------------------------------------------------

    struct Env {
        api_base: String,
        token: Option<String>,
        path: OsString,
        target: String,
        exe: PathBuf,
        current: Version,
        /// The running build's own version text (a dev build's `+dev.N` included).
        current_text: String,
        user_agent: String,
    }

    pub(super) async fn run(args: UpgradeArgs) -> Result<()> {
        let info = vam_spec::build_info!();
        let current = Version::parse(&info.version, true)
            .with_context(|| format!("this vam's own version is {}", info.version))?;
        let exe = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .context("find the running vam")?;
        let token = ["GH_TOKEN", "GITHUB_TOKEN"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
        let env = Env {
            api_base: api_base()?,
            token,
            path: std::env::var_os("PATH").unwrap_or_default(),
            target: env!("VAM_TARGET").to_string(),
            exe,
            current,
            current_text: info.version.clone(),
            user_agent: format!("vam/{}", info.version),
        };
        execute(&env, &args, &mut io::stdout().lock()).await
    }

    async fn execute<W: Write>(env: &Env, args: &UpgradeArgs, out: &mut W) -> Result<()> {
        // 1. Arguments, 2. the machine: nothing here touches the network or a file.
        let pinned = args.version.is_some();
        let sel = match (&args.version, args.pre) {
            (Some(v), _) => Selection::Pin(Version::release(v)?),
            (None, true) => Selection::Pre,
            (None, false) => Selection::Final,
        };
        let bundle = in_node_bundle(&env.exe);
        if bundle && !args.check {
            bail!(
                "{} sits beside vamd, so it belongs to a node bundle: upgrade the node from its \
                 bundle (extract the new node bundle and run `sudo ./vam node init` from it). \
                 vam upgrade only replaces a standalone vam",
                env.exe.display()
            );
        }
        let triple = asset_triple(&env.target)?;
        if !args.check {
            let dir = env.exe.parent().unwrap_or(Path::new("."));
            probe_writable(dir)?;
        }
        let cur = env.current_text.as_str();
        writeln!(out, "vam {cur} at {}", env.exe.display())?;
        if env.current.dev {
            writeln!(
                out,
                "this is a dev build: it counts as newer than the release it grew from, so a \
                 plain upgrade only moves to a later release; --version vX replaces it with \
                 that release"
            )?;
        }
        if bundle {
            writeln!(
                out,
                "note: vamd is beside this vam (a node bundle): vam upgrade will refuse here; \
                 upgrade the node from its bundle"
            )?;
        }

        // 3. Which release.
        let api = Api::new(&env.api_base, env.token.clone(), &env.user_agent)?;
        let (target, release) = resolve(&api, &sel).await?;
        let order = target.cmp(&env.current);
        let how = if pinned {
            format!("vam upgrade --version {}", target.tag())
        } else if release.prerelease || args.pre {
            "vam upgrade --pre".to_string()
        } else {
            "vam upgrade".to_string()
        };
        match order {
            Ordering::Equal => {
                writeln!(out, "vam {cur} is up to date ({})", target.tag())?;
                return Ok(());
            }
            Ordering::Less if !pinned => {
                if args.check {
                    writeln!(
                        out,
                        "vam {cur} is newer than the latest release {}; nothing to do \
                         (`vam upgrade --version {}` would install it anyway)",
                        target.tag(),
                        target.tag()
                    )?;
                    return Ok(());
                }
                bail!(
                    "vam {cur} is newer than the latest release {}; refusing to downgrade \
                     (pass --version {} to install it anyway)",
                    target.tag(),
                    target.tag()
                );
            }
            _ => {}
        }
        let verb = if order == Ordering::Less {
            "a downgrade to"
        } else {
            "upgrade to"
        };
        if args.check {
            writeln!(out, "{verb} {} is available: run `{how}`", target.tag())?;
            return Ok(());
        }

        // 4. Assets, 5. downloads.
        let archive_name = asset_name(&target, triple);
        let archive_asset = release.asset(&archive_name)?;
        let sums_asset = release.asset("SHA256SUMS")?;
        writeln!(out, "{verb} {}: downloading SHA256SUMS", target.tag())?;
        let sums = api
            .download(&sums_asset.browser_download_url, MAX_SUMS, "SHA256SUMS")
            .await?;
        writeln!(out, "downloading {archive_name}")?;
        let archive = api
            .download(
                &archive_asset.browser_download_url,
                MAX_ARCHIVE,
                &archive_name,
            )
            .await?;

        // 6. The hash, always.
        let sums = String::from_utf8_lossy(&sums);
        let Some(listed) = listed_hash(&sums, &archive_name) else {
            return Err(fail(
                ExitCode::VerifyFailed,
                format!("SHA256SUMS does not list {archive_name}; nothing was replaced"),
            ));
        };
        let got = sha256_hex(&archive);
        if got != listed {
            return Err(fail(
                ExitCode::VerifyFailed,
                format!(
                    "hash mismatch for {archive_name}: SHA256SUMS lists {listed}, the download \
                     is {got}; nothing was replaced"
                ),
            ));
        }
        writeln!(out, "hash ok (sha256 {got})")?;

        // 7. The attestation, when gh is here.
        match find_in_path("gh", &env.path) {
            Some(gh) => {
                let dir = tempfile::tempdir().context("create a temporary directory")?;
                let file = dir.path().join(&archive_name);
                fs::write(&file, &archive).context("write the archive for gh")?;
                verify_attestation(&gh, &file).await?;
                writeln!(out, "attestation verified with gh")?;
            }
            None => {
                let mut cmd = attest_command();
                cmd[2] = archive_name.clone();
                writeln!(
                    out,
                    "attestation not checked: gh is not on the PATH. To check it, install gh, \
                     download {archive_name} from the release, and run: gh {}",
                    cmd.join(" ")
                )?;
            }
        }

        // 8. Extract beside the running file, 9. smoke-run it, 10. rename over it.
        let staged = stage(&env.exe, &archive)?;
        let line = smoke(&staged.0, &target).await?;
        replace(&staged, &env.exe)?;
        writeln!(
            out,
            "upgraded vam {cur} -> {} ({line}) at {}",
            target.tag(),
            env.exe.display()
        )?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn v(s: &str) -> Version {
            Version::parse(s, true).unwrap()
        }

        #[test]
        fn versions_order_by_semver() {
            assert!(v("0.1.0-rc.2") < v("0.1.0-rc.10"));
            assert!(v("0.1.0-rc.10") < v("0.1.0"));
            assert!(v("0.1.0") < v("0.1.1-rc.1"));
            assert!(v("0.1.0-alpha") < v("0.1.0-alpha.1"));
            assert!(v("0.1.0-1") < v("0.1.0-alpha"));
            assert!(v("9.9.9") > v("0.10.3"));
            assert_eq!(v("v1.2.3"), v("1.2.3"));
        }

        #[test]
        fn a_dev_build_is_newer_than_its_tag_and_older_than_the_next() {
            assert!(v("0.3.0+dev.14") > v("0.3.0"));
            assert!(v("0.3.0+dev.14") < v("0.3.1"));
            assert!(v("0.1.0-rc.1+dev.3") > v("0.1.0-rc.1"));
            assert!(v("0.1.0-rc.1+dev.3") < v("0.1.0-rc.2"));
            assert!(v("0.1.0+dev").dev);
        }

        #[test]
        fn bad_versions_are_refused() {
            for s in [
                "1.2",
                "1.2.3.4",
                "vX",
                "",
                "1.2.x",
                "01.2.3",
                "1.2.3-",
                "1.2.3-a..b",
                "1.2.3-01",
                "1.2.3 ",
                "1.2.3+build5",
            ] {
                assert!(Version::release(s).is_err(), "{s}");
            }
            // build metadata is only accepted for the running build's own version
            assert!(Version::release("0.3.0+dev.14").is_err());
            assert!(Version::parse("0.3.0+dev.x", true).is_err());
            assert!(Version::parse("0.3.0+other", true).is_err());
        }

        #[test]
        fn asset_names_match_package_sh() {
            assert_eq!(
                asset_name(&v("v0.1.0-rc.2"), "aarch64-apple-darwin"),
                "vam-0.1.0-rc.2-aarch64-apple-darwin.tar.gz"
            );
            assert_eq!(v("0.1.0-rc.2").tag(), "v0.1.0-rc.2");
        }

        #[test]
        fn only_shipped_targets_have_an_asset() {
            assert!(asset_triple("x86_64-unknown-linux-gnu").is_ok());
            assert!(asset_triple("aarch64-apple-darwin").is_ok());
            let e = asset_triple("x86_64-unknown-linux-musl").unwrap_err();
            assert!(e.to_string().contains("no release asset"));
        }

        #[test]
        fn sha256sums_lines_parse_in_both_forms() {
            let sums = "AAAA  other.tar.gz\nabc123  vam-1.tar.gz\ndef456 *vam-2.tar.gz\n";
            assert_eq!(listed_hash(sums, "vam-1.tar.gz").as_deref(), Some("abc123"));
            assert_eq!(listed_hash(sums, "vam-2.tar.gz").as_deref(), Some("def456"));
            assert_eq!(listed_hash(sums, "vam-3.tar.gz"), None);
            assert_eq!(listed_hash(sums, "vam-1.tar"), None);
        }

        #[test]
        fn the_override_url_must_be_loopback() {
            assert!(is_loopback_url("http://127.0.0.1:8080"));
            assert!(is_loopback_url("http://localhost:1"));
            assert!(is_loopback_url("http://[::1]:3"));
            assert!(!is_loopback_url("https://127.0.0.1:8080"));
            assert!(!is_loopback_url("http://example.com"));
            assert!(!is_loopback_url("http://127.0.0.1.example.com"));
            assert!(!is_loopback_url("nonsense"));
        }

        fn tgz(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
            let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            let mut b = tar::Builder::new(gz);
            for (name, body, kind) in entries {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(*kind);
                h.set_size(body.len() as u64);
                h.set_mode(0o755);
                h.set_cksum();
                if *kind == tar::EntryType::Symlink {
                    h.set_size(0);
                    b.append_link(&mut h, name, "/bin/sh").unwrap();
                } else {
                    b.append_data(&mut h, name, *body).unwrap();
                }
            }
            b.into_inner().unwrap().finish().unwrap()
        }

        fn file(name: &str, body: &[u8]) -> (String, Vec<u8>, tar::EntryType) {
            (name.to_string(), body.to_vec(), tar::EntryType::Regular)
        }

        fn extracted(archive: &[u8]) -> Result<Vec<u8>> {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("out");
            let mut f = fs::File::create(&p).unwrap();
            extract_vam(archive, &mut f)?;
            Ok(fs::read(p).unwrap())
        }

        fn pack(items: &[(String, Vec<u8>, tar::EntryType)]) -> Vec<u8> {
            let v: Vec<(&str, &[u8], tar::EntryType)> = items
                .iter()
                .map(|(n, b, k)| (n.as_str(), b.as_slice(), *k))
                .collect();
            tgz(&v)
        }

        #[test]
        fn extract_takes_vam_at_the_top_or_one_directory_down() {
            assert_eq!(extracted(&pack(&[file("vam", b"A")])).unwrap(), b"A");
            let nested = pack(&[
                file("vam-1.0.0-x/NOTICE", b"n"),
                file("vam-1.0.0-x/vam", b"B"),
            ]);
            assert_eq!(extracted(&nested).unwrap(), b"B");
            assert_eq!(extracted(&pack(&[file("./vam", b"C")])).unwrap(), b"C");
        }

        #[test]
        fn extract_ignores_odd_paths_and_refuses_odd_entries() {
            let none = pack(&[file("a/b/vam", b"deep"), file("vam-extra", b"x")]);
            assert!(extracted(&none)
                .unwrap_err()
                .to_string()
                .contains("no `vam`"));
            assert!(extracted(&pack(&[file("vam", b"")]))
                .unwrap_err()
                .to_string()
                .contains("empty"));
            let link = tgz(&[("vam", b"", tar::EntryType::Symlink)]);
            assert!(extracted(&link)
                .unwrap_err()
                .to_string()
                .contains("regular file"));
            assert!(extracted(b"not gzip").is_err());
        }

        #[test]
        fn a_traversal_path_is_never_taken_for_vam() {
            // The tar builder refuses to write `..`, so build the header by hand.
            let mut raw = Vec::new();
            {
                let mut b = tar::Builder::new(&mut raw);
                let mut h = tar::Header::new_gnu();
                h.set_size(1);
                h.set_mode(0o755);
                let name = b"../vam";
                h.as_old_mut().name[..name.len()].copy_from_slice(name);
                h.set_cksum();
                b.append(&h, &b"x"[..]).unwrap();
                b.finish().unwrap();
            }
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            gz.write_all(&raw).unwrap();
            let archive = gz.finish().unwrap();
            assert!(extracted(&archive)
                .unwrap_err()
                .to_string()
                .contains("no `vam`"));
        }

        #[test]
        fn find_in_path_wants_an_executable_file() {
            let d = tempfile::tempdir().unwrap();
            let gh = d.path().join("gh");
            fs::write(&gh, "#!/bin/sh\n").unwrap();
            assert!(find_in_path("gh", d.path().as_os_str()).is_none());
            fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(find_in_path("gh", d.path().as_os_str()), Some(gh));
            assert!(find_in_path("gh", OsStr::new("")).is_none());
            assert!(find_in_path("gh", OsStr::new("/nonexistent-dir")).is_none());
        }

        #[test]
        fn a_gh_too_old_for_the_check_is_named_and_still_refused() {
            let old = attest_refusal(
                "unknown command \"attestation\" for \"gh\"\n\nUsage:  gh <command>",
            );
            assert!(
                old.contains("too old") && old.contains("gh 2.51.0 or newer"),
                "{old}"
            );
            assert!(old.contains("Nothing was replaced"), "{old}");
            let no_flag = attest_refusal("unknown flag: --signer-workflow\n");
            assert!(no_flag.contains("gh 2.51.0 or newer"), "{no_flag}");
            let refused = attest_refusal("X no matching attestations found\n");
            assert!(refused.contains("refused the download"), "{refused}");
            assert!(!refused.contains("too old"), "{refused}");
        }

        #[test]
        fn a_token_is_sent_only_to_the_official_host() {
            let tok = Some("t0k".to_string());
            let official = Api::new(API_BASE, tok.clone(), "x").unwrap();
            assert_eq!(official.authorization().as_deref(), Some("Bearer t0k"));
            let other = Api::new("http://127.0.0.1:1", tok, "x").unwrap();
            assert_eq!(other.authorization(), None);
            assert_eq!(Api::new(API_BASE, None, "x").unwrap().authorization(), None);
        }

        #[test]
        fn downloads_are_limited_to_github_and_the_api_host() {
            let api = Api::new(API_BASE, None, "x").unwrap();
            assert!(api
                .check_download_url("https://github.com/squrell-inc/VAM/releases/download/v1/a")
                .is_ok());
            assert!(api.check_download_url("https://api.github.com/x").is_ok());
            assert!(api.check_download_url("http://github.com/x").is_err());
            assert!(api.check_download_url("https://evil.example/x").is_err());
            assert!(api
                .check_download_url("https://github.com.evil.example/x")
                .is_err());
            let local = Api::new("http://127.0.0.1:9", None, "x").unwrap();
            assert!(local
                .check_download_url("http://127.0.0.1:9/assets/a")
                .is_ok());
            assert!(local
                .check_download_url("http://127.0.0.1:10/assets/a")
                .is_err());
        }

        // ---- the command against a mock release API ----

        type Routes =
            std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, (u16, Vec<u8>)>>>;

        /// A loopback HTTP stub: GET path -> (status, body); anything else is a 404.
        struct Stub {
            base: String,
            routes: Routes,
            stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }

        impl Stub {
            fn start() -> Self {
                use std::sync::atomic::Ordering;
                let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                listener.set_nonblocking(true).unwrap();
                let base = format!("http://{}", listener.local_addr().unwrap());
                let routes: Routes = Default::default();
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let (r, st) = (routes.clone(), stop.clone());
                std::thread::spawn(move || {
                    while !st.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((mut conn, _)) => {
                                let _ = conn.set_nonblocking(false);
                                let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
                                let mut buf = [0u8; 4096];
                                let n = conn.read(&mut buf).unwrap_or(0);
                                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                                let mut it =
                                    req.lines().next().unwrap_or_default().split_whitespace();
                                let (method, path) =
                                    (it.next().unwrap_or_default(), it.next().unwrap_or_default());
                                let path = path.split('?').next().unwrap_or_default();
                                let hit = if method == "GET" {
                                    r.lock().unwrap().get(path).cloned()
                                } else {
                                    None
                                };
                                let (code, body) = hit.unwrap_or((404, b"{}".to_vec()));
                                let _ = write!(
                                    conn,
                                    "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    body.len()
                                );
                                let _ = conn.write_all(&body);
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(5)),
                        }
                    }
                });
                Stub { base, routes, stop }
            }

            fn route(&self, path: impl Into<String>, status: u16, body: impl AsRef<[u8]>) {
                self.routes
                    .lock()
                    .unwrap()
                    .insert(path.into(), (status, body.as_ref().to_vec()));
            }

            fn base_url(&self) -> String {
                self.base.clone()
            }

            fn url(&self, path: &str) -> String {
                format!("{}{path}", self.base)
            }
        }

        impl Drop for Stub {
            fn drop(&mut self) {
                self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        const TARGET: &str = "x86_64-unknown-linux-gnu";

        fn script(version_line: &str, code: i32) -> Vec<u8> {
            format!("#!/bin/sh\necho '{version_line}'\nexit {code}\n").into_bytes()
        }

        struct Fx {
            dir: tempfile::TempDir,
            bin: PathBuf,
            path: PathBuf,
            server: Stub,
        }

        impl Fx {
            fn new() -> Self {
                let dir = tempfile::tempdir().unwrap();
                let bin = dir.path().join("bin");
                let path = dir.path().join("path");
                fs::create_dir_all(&bin).unwrap();
                fs::create_dir_all(&path).unwrap();
                let exe = bin.join("vam");
                fs::write(&exe, script("vam 0.1.0 (old)", 0)).unwrap();
                fs::set_permissions(&exe, fs::Permissions::from_mode(0o750)).unwrap();
                Fx {
                    dir,
                    bin,
                    path,
                    server: Stub::start(),
                }
            }

            fn exe(&self) -> PathBuf {
                self.bin.join("vam")
            }

            fn env(&self, current: &str) -> Env {
                Env {
                    api_base: self.server.base_url(),
                    token: None,
                    path: self.path.clone().into_os_string(),
                    target: TARGET.into(),
                    exe: self.exe(),
                    current: v(current),
                    current_text: current.to_string(),
                    user_agent: "vam/test".into(),
                }
            }

            /// Serves one release holding the archive, with `listed` as the hash line.
            fn release(&self, tag: &str, prerelease: bool, archive: &[u8], sum: Option<&str>) {
                let name = format!("vam-{}-{TARGET}.tar.gz", tag.trim_start_matches('v'));
                let real = sha256_hex(archive);
                let sums = format!("{}  {name}\n", sum.unwrap_or(&real));
                self.release_raw(tag, prerelease, archive, &sums);
            }

            fn release_raw(&self, tag: &str, prerelease: bool, archive: &[u8], sums: &str) {
                let ver = tag.trim_start_matches('v');
                let name = format!("vam-{ver}-{TARGET}.tar.gz");
                let sums = sums.to_string();
                let (a, s) = (format!("/assets/{name}"), "/assets/SHA256SUMS".to_string());
                self.server.route(a.clone(), 200, archive);
                self.server.route(s.clone(), 200, sums.clone());
                let rel = serde_json::json!({
                    "tag_name": tag, "prerelease": prerelease, "draft": false,
                    "assets": [
                        {"name": name, "browser_download_url": self.server.url(&a)},
                        {"name": "SHA256SUMS", "browser_download_url": self.server.url(&s)},
                    ],
                });
                let (base, t) = (format!("/repos/{REPO}/releases"), tag.to_string());
                let body = rel.to_string();
                self.server
                    .route(format!("{base}/tags/{t}"), 200, body.clone());
                if !prerelease {
                    self.server
                        .route(format!("{base}/latest"), 200, body.clone());
                }
                let body = format!("[{body}]");
                self.server.route(base.clone(), 200, body.clone());
            }

            fn stray_files(&self) -> Vec<String> {
                let mut v: Vec<String> = fs::read_dir(&self.bin)
                    .unwrap()
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n != "vam")
                    .collect();
                v.sort();
                v
            }
        }

        fn args(check: bool, version: Option<&str>, pre: bool) -> UpgradeArgs {
            UpgradeArgs {
                check,
                version: version.map(str::to_string),
                pre,
            }
        }

        async fn exec(fx: &Fx, current: &str, a: UpgradeArgs) -> (Result<()>, String) {
            let mut out = Vec::new();
            let r = execute(&fx.env(current), &a, &mut out).await;
            (r, String::from_utf8(out).unwrap())
        }

        fn new_vam() -> Vec<u8> {
            pack(&[file("vam-1.0.0-x/vam", &script("vam 1.0.0 (new)", 0))])
        }

        #[tokio::test]
        async fn upgrade_replaces_the_binary_and_keeps_its_mode() {
            let fx = Fx::new();
            fx.release("v1.0.0", false, &new_vam(), None);
            let (r, out) = exec(&fx, "0.1.0", args(false, None, false)).await;
            r.unwrap();
            assert!(out.contains("hash ok"), "{out}");
            assert!(out.contains("attestation not checked"), "{out}");
            assert!(out.contains("gh"), "{out}");
            assert!(out.contains("upgraded vam 0.1.0 -> v1.0.0"), "{out}");
            assert!(fs::read_to_string(fx.exe())
                .unwrap()
                .contains("vam 1.0.0 (new)"));
            let mode = fs::metadata(fx.exe()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o750);
            assert!(fx.stray_files().is_empty(), "{:?}", fx.stray_files());
        }

        #[tokio::test]
        async fn a_wrong_hash_stops_before_anything_is_written() {
            let fx = Fx::new();
            let archive = new_vam();
            let bad = "0".repeat(64);
            fx.release("v1.0.0", false, &archive, Some(&bad));
            let before = fs::read(fx.exe()).unwrap();
            let (r, _) = exec(&fx, "0.1.0", args(false, None, false)).await;
            let e = r.unwrap_err().to_string();
            assert!(e.contains("hash mismatch"), "{e}");
            assert_eq!(fs::read(fx.exe()).unwrap(), before);
            assert!(fx.stray_files().is_empty());
        }

        #[tokio::test]
        async fn a_missing_hash_line_stops_too() {
            let fx = Fx::new();
            let archive = new_vam();
            fx.release_raw("v1.0.0", false, &archive, "abc  other.tar.gz\n");
            let before = fs::read(fx.exe()).unwrap();
            let (r, _) = exec(&fx, "0.1.0", args(false, None, false)).await;
            assert!(r.is_err());
            assert_eq!(fs::read(fx.exe()).unwrap(), before);
        }

        #[tokio::test]
        async fn a_binary_that_does_not_start_or_misreports_never_replaces_the_old_one() {
            for (body, why) in [
                (script("vam 1.0.0 (new)", 3), "exit status"),
                (script("vam 2.0.0 (new)", 0), "wrong version"),
                (
                    script("vam 1.0.0-rc.1 (new)", 0),
                    "prefix of another version",
                ),
                (b"not a program".to_vec(), "not executable code"),
            ] {
                let fx = Fx::new();
                fx.release("v1.0.0", false, &pack(&[file("vam", &body)]), None);
                let before = fs::read(fx.exe()).unwrap();
                let (r, _) = exec(&fx, "0.1.0", args(false, None, false)).await;
                assert!(r.is_err(), "{why}");
                assert_eq!(fs::read(fx.exe()).unwrap(), before, "{why}");
                assert!(fx.stray_files().is_empty(), "{why}: {:?}", fx.stray_files());
            }
        }

        #[tokio::test]
        async fn check_reads_only() {
            let fx = Fx::new();
            fx.release("v1.0.0", false, &new_vam(), None);
            let before = fs::read(fx.exe()).unwrap();
            let (r, out) = exec(&fx, "0.1.0", args(true, None, false)).await;
            r.unwrap();
            assert!(
                out.contains("v1.0.0") && out.contains("`vam upgrade`"),
                "{out}"
            );
            assert_eq!(fs::read(fx.exe()).unwrap(), before);
            assert!(fx.stray_files().is_empty());
            assert!(!out.contains("downloading"), "{out}");
        }

        #[tokio::test]
        async fn up_to_date_and_downgrade_rules() {
            let fx = Fx::new();
            fx.release("v1.0.0", false, &new_vam(), None);
            let before = fs::read(fx.exe()).unwrap();

            let (r, out) = exec(&fx, "1.0.0", args(false, None, false)).await;
            r.unwrap();
            assert!(out.contains("up to date"), "{out}");

            let (r, _) = exec(&fx, "2.0.0", args(false, None, false)).await;
            let e = r.unwrap_err().to_string();
            assert!(
                e.contains("refusing to downgrade") && e.contains("--version v1.0.0"),
                "{e}"
            );
            assert_eq!(fs::read(fx.exe()).unwrap(), before);

            let (r, out) = exec(&fx, "2.0.0", args(true, None, false)).await;
            r.unwrap();
            assert!(out.contains("newer than the latest release"), "{out}");

            // a dev build ahead of the latest tag is not moved back either
            let (r, out) = exec(&fx, "1.0.0+dev.4", args(false, None, false)).await;
            assert!(r.is_err());
            assert!(out.contains("dev build"), "{out}");

            // pinned: a downgrade is allowed
            let (r, out) = exec(&fx, "2.0.0", args(false, Some("v1.0.0"), false)).await;
            r.unwrap();
            assert!(out.contains("a downgrade to v1.0.0"), "{out}");
            assert!(fs::read_to_string(fx.exe()).unwrap().contains("(new)"));
        }

        #[tokio::test]
        async fn without_a_final_release_it_says_so() {
            let fx = Fx::new();
            fx.release("v1.0.0-rc.1", true, &new_vam(), None);
            let (r, _) = exec(&fx, "0.1.0", args(false, None, false)).await;
            let e = r.unwrap_err().to_string();
            assert!(
                e.contains("no final release yet") && e.contains("--pre"),
                "{e}"
            );
            let (r, out) = exec(&fx, "0.1.0", args(true, None, true)).await;
            r.unwrap();
            assert!(
                out.contains("v1.0.0-rc.1") && out.contains("vam upgrade --pre"),
                "{out}"
            );
        }

        #[tokio::test]
        async fn pre_picks_the_highest_published_release_and_skips_drafts() {
            let fx = Fx::new();
            let rel = |tag: &str, draft: bool| serde_json::json!({"tag_name": tag, "prerelease": true, "draft": draft, "assets": []});
            let list = serde_json::json!([
                rel("v0.2.0-rc.1", false),
                rel("v0.3.0-rc.1", true),
                rel("v0.2.0-rc.10", false),
                rel("v0.2.0-rc.9", false),
                rel("nightly", false),
            ])
            .to_string();
            fx.server
                .route(format!("/repos/{REPO}/releases"), 200, list.clone());
            let (r, out) = exec(&fx, "0.1.0", args(true, None, true)).await;
            r.unwrap();
            assert!(out.contains("v0.2.0-rc.10"), "{out}");
        }

        #[tokio::test]
        async fn an_unknown_pin_is_an_error() {
            let fx = Fx::new();
            let (r, _) = exec(&fx, "0.1.0", args(false, Some("v7.7.7"), false)).await;
            assert!(r
                .unwrap_err()
                .to_string()
                .contains("no published release v7.7.7"));
            let (r, _) = exec(&fx, "0.1.0", args(false, Some("7.7"), false)).await;
            assert!(r.unwrap_err().to_string().contains("not a release version"));
        }

        #[tokio::test]
        async fn a_release_without_the_asset_for_this_target_stops_before_downloading() {
            let fx = Fx::new();
            let body = serde_json::json!({"tag_name": "v1.0.0", "prerelease": false, "assets": []})
                .to_string();
            fx.server
                .route(format!("/repos/{REPO}/releases/latest"), 200, body.clone());
            let (r, _) = exec(&fx, "0.1.0", args(false, None, false)).await;
            assert!(r
                .unwrap_err()
                .to_string()
                .contains("has no asset named vam-1.0.0-"));
        }

        #[tokio::test]
        async fn an_unshipped_target_stops_before_the_network() {
            let fx = Fx::new();
            let mut env = fx.env("0.1.0");
            env.target = "x86_64-unknown-linux-musl".into();
            let mut out = Vec::new();
            let r = execute(&env, &args(false, None, false), &mut out).await;
            assert!(r.unwrap_err().to_string().contains("no release asset"));
            assert!(out.is_empty());
        }

        #[tokio::test]
        async fn inside_a_node_bundle_it_refuses_but_check_reports() {
            let fx = Fx::new();
            fx.release("v1.0.0", false, &new_vam(), None);
            fs::write(fx.bin.join("vamd"), "x").unwrap();
            let before = fs::read(fx.exe()).unwrap();
            let (r, _) = exec(&fx, "0.1.0", args(false, None, false)).await;
            let e = r.unwrap_err().to_string();
            assert!(e.contains("upgrade the node from its bundle"), "{e}");
            assert_eq!(fs::read(fx.exe()).unwrap(), before);
            let (r, out) = exec(&fx, "0.1.0", args(true, None, false)).await;
            r.unwrap();
            assert!(
                out.contains("node bundle") && out.contains("v1.0.0"),
                "{out}"
            );
            assert!(fx.bin.join("vamd").is_file());
        }

        #[tokio::test]
        async fn an_unwritable_install_dir_is_named_before_any_download() {
            let fx = Fx::new();
            fx.release("v1.0.0", false, &new_vam(), None);
            fs::set_permissions(&fx.bin, fs::Permissions::from_mode(0o555)).unwrap();
            if fs::File::create(fx.bin.join("root-probe")).is_ok() {
                return; // running as root: it can write anywhere
            }
            let (r, out) = exec(&fx, "0.1.0", args(false, None, false)).await;
            fs::set_permissions(&fx.bin, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(r.unwrap_err().to_string().contains("cannot write to"));
            assert!(!out.contains("downloading"));
        }

        fn fake_gh(fx: &Fx, code: i32) {
            let gh = fx.path.join("gh");
            fs::write(
                &gh,
                format!(
                    "#!/bin/sh\necho \"gh $*\" > '{}/gh-args'\necho 'no match' >&2\nexit {code}\n",
                    fx.dir.path().display()
                ),
            )
            .unwrap();
            fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        }

        #[tokio::test]
        async fn gh_that_verifies_lets_the_upgrade_go_on() {
            let fx = Fx::new();
            fx.release("v1.0.0", false, &new_vam(), None);
            fake_gh(&fx, 0);
            let (r, out) = exec(&fx, "0.1.0", args(false, None, false)).await;
            r.unwrap();
            assert!(out.contains("attestation verified"), "{out}");
            let seen = fs::read_to_string(fx.dir.path().join("gh-args")).unwrap();
            assert!(seen.contains("attestation verify"), "{seen}");
            assert!(seen.contains("-R squrell-inc/VAM"), "{seen}");
            assert!(
                seen.contains("--signer-workflow squrell-inc/VAM/.github/workflows/release.yml"),
                "{seen}"
            );
            assert!(fs::read_to_string(fx.exe()).unwrap().contains("(new)"));
        }

        #[tokio::test]
        async fn gh_that_refuses_stops_the_upgrade_and_leaves_vam_alone() {
            let fx = Fx::new();
            fx.release("v1.0.0", false, &new_vam(), None);
            fake_gh(&fx, 1);
            let before = fs::read(fx.exe()).unwrap();
            let (r, _) = exec(&fx, "0.1.0", args(false, None, false)).await;
            let e = format!("{:#}", r.unwrap_err());
            assert!(e.contains("attestation") && e.contains("no match"), "{e}");
            assert_eq!(fs::read(fx.exe()).unwrap(), before);
            assert!(fx.stray_files().is_empty());
        }

        #[tokio::test]
        async fn api_errors_are_reported() {
            let fx = Fx::new();
            fx.server
                .route(format!("/repos/{REPO}/releases/latest"), 403, "{}");
            let (r, _) = exec(&fx, "0.1.0", args(true, None, false)).await;
            let e = r.unwrap_err().to_string();
            assert!(e.contains("403") && e.contains("GH_TOKEN"), "{e}");
        }
    }
}
