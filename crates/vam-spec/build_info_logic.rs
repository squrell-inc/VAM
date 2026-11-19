//! Build-info rules, std only. Shared by the binary crates' `build.rs` files (through
//! `#[path]`) and by `vam-spec`'s unit tests, so the rules exist once. Library crates never read
//! git; each binary crate's build script calls `emit_build_info()`.

use std::path::Path;
use std::process::Command;

/// The channel name for a release build.
pub const CHANNEL_RELEASE: &str = "release";
/// The channel name for every other build.
pub const CHANNEL_DEV: &str = "dev";

/// `release` only when `VAM_BUILD_CHANNEL=release`; anything else is `dev`.
pub fn resolve_channel(env_value: Option<&str>) -> &'static str {
    if env_value.map(str::trim) == Some(CHANNEL_RELEASE) {
        CHANNEL_RELEASE
    } else {
        CHANNEL_DEV
    }
}

/// The version string. A release build is `pkg_version` exactly. A dev build is the nearest tag
/// plus `+dev.<N>` (`git describe --tags --long` output in `describe`), or `<pkg_version>+dev`
/// when no tag is reachable.
pub fn resolve_version(channel: &str, pkg_version: &str, describe: Option<&str>) -> String {
    if channel == CHANNEL_RELEASE {
        return pkg_version.to_string();
    }
    match describe.and_then(parse_describe) {
        Some((tag, n)) => format!("{tag}+dev.{n}"),
        None => format!("{pkg_version}+dev"),
    }
}

/// `v0.3.0-14-g9be41d0` -> (`0.3.0`, 14). A tag may itself hold dashes (`v0.1.0-rc.1-3-gabc`).
fn parse_describe(describe: &str) -> Option<(String, u32)> {
    let mut parts = describe.trim().rsplitn(3, '-');
    let sha = parts.next()?;
    let n = parts.next()?.parse::<u32>().ok()?;
    let tag = parts.next()?;
    if !sha.starts_with('g') {
        return None;
    }
    let tag = tag.strip_prefix('v')?;
    (!tag.is_empty()).then(|| (tag.to_string(), n))
}

/// The commit: the override as given, else the short SHA (`+dirty` when the tree has changes),
/// else `unknown`.
pub fn resolve_commit(override_sha: Option<&str>, sha: Option<&str>, dirty: bool) -> String {
    if let Some(o) = override_sha.map(str::trim).filter(|o| !o.is_empty()) {
        return o.to_string();
    }
    match sha {
        Some(s) if dirty => format!("{s}+dirty"),
        Some(s) => s.to_string(),
        None => "unknown".to_string(),
    }
}

/// The commit date: the override, else the git date, else empty.
pub fn resolve_date(override_date: Option<&str>, git_date: Option<&str>) -> String {
    override_date
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .or(git_date)
        .unwrap_or("")
        .to_string()
}

fn git_output(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn git_succeeds(args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A git-dir path (`HEAD`, `index`, a ref) that exists. A missing `rerun-if-changed` path makes
/// cargo rerun every time, so missing ones are skipped.
fn git_path(name: &str) -> Option<String> {
    let path = git_output(&["rev-parse", "--git-path", name])?;
    Path::new(&path).exists().then_some(path)
}

fn env_opt(name: &str) -> Option<String> {
    println!("cargo:rerun-if-env-changed={name}");
    std::env::var(name).ok()
}

/// Run git and print the `cargo:` lines a binary crate's build script needs:
/// `VAM_VERSION`, `VAM_GIT_SHA`, `VAM_COMMIT_DATE`, `VAM_BUILD_CHANNEL`. Never fails the build.
pub fn emit_build_info() {
    println!("cargo:rerun-if-changed=build.rs");
    let channel_env = env_opt("VAM_BUILD_CHANNEL");
    let sha_env = env_opt("VAM_GIT_SHA");
    let date_env = env_opt("VAM_COMMIT_DATE");
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();

    let channel = resolve_channel(channel_env.as_deref());
    let describe = git_output(&["describe", "--tags", "--long", "--match", "v[0-9]*"]);
    let sha = git_output(&["rev-parse", "--short=7", "HEAD"]);
    let dirty = sha.is_some() && !git_succeeds(&["diff", "--quiet", "HEAD"]);
    let git_date = git_output(&["show", "-s", "--format=%cs", "HEAD"]);

    let version = resolve_version(channel, &pkg, describe.as_deref());
    let commit = resolve_commit(sha_env.as_deref(), sha.as_deref(), dirty);
    let date = resolve_date(date_env.as_deref(), git_date.as_deref());
    println!("cargo:rustc-env=VAM_VERSION={version}");
    println!("cargo:rustc-env=VAM_GIT_SHA={commit}");
    println!("cargo:rustc-env=VAM_COMMIT_DATE={date}");
    println!("cargo:rustc-env=VAM_BUILD_CHANNEL={channel}");

    for path in [git_path("HEAD"), git_path("index"), git_path("packed-refs")]
        .into_iter()
        .flatten()
    {
        println!("cargo:rerun-if-changed={path}");
    }
    if let Some(reference) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git_path(&reference) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(path) = git_path("refs/tags") {
        println!("cargo:rerun-if-changed={path}");
    }
}
