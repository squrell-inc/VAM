//! Embeds the short git SHA (and a `+dirty` marker) in `vam --version`.
//!
//! Falls back to `unknown` when git is unavailable or the crate is built
//! outside a checkout, so a build never fails on the version string.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");

    let sha = git_output(&["rev-parse", "--short", "HEAD"]);
    let version = match sha {
        Some(sha) => {
            let dirty = !git_succeeds(&["diff", "--quiet", "HEAD"]);
            if dirty {
                format!("{sha}+dirty")
            } else {
                sha
            }
        }
        None => "unknown".to_string(),
    };
    println!("cargo:rustc-env=VAM_GIT_SHA={version}");

    emit_git_rerun_triggers();
}

/// Re-run this script when HEAD moves, when the ref it points at changes, or
/// when the index changes, so the embedded SHA does not go stale.
fn emit_git_rerun_triggers() {
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
}

/// Resolve a git-dir path (`HEAD`, `index`, a ref) to a file that exists, so
/// worktrees and packed refs work. Missing files are skipped: cargo treats a
/// missing `rerun-if-changed` path as always changed.
fn git_path(name: &str) -> Option<String> {
    let path = git_output(&["rev-parse", "--git-path", name])?;
    Path::new(&path).exists().then_some(path)
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
