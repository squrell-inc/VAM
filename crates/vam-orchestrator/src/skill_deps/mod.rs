//! Skill dependencies (VAM image I1): read a skill's lockfile (VI-1.4) and fetch the packages it
//! locks into a content-addressed cache through the operator's mirrors (VI-1.5).
//!
//! The lockfile chooses what (name, version, hash); the operator chooses where. A URL inside a
//! lockfile is never fetched. Nothing here runs a subprocess, and no package code runs on the
//! host: archives are unpacked by safe, link-free unpackers after their hash matches. The API is
//! blocking; callers use `spawn_blocking` (`reqwest::blocking` panics in an async context).
//!
//! The pure half (`names`, `marker`, `wheel_tags`, `requirements`, `pylock`, `npm`, `plan`, `target`,
//! `limits`) does no I/O; a test greps those files for it.

mod cache;
mod files;
mod index;
mod limits;
mod marker;
mod mirrors;
mod names;
mod npm;
mod plan;
mod pylock;
mod requirements;
mod resolve;
mod target;
mod transport;
mod unpack;
mod wheel_tags;

#[cfg(test)]
mod testutil;

pub use files::SkillFiles;
pub use limits::Limits;
pub use mirrors::Mirrors;
pub use plan::plan;
pub use resolve::{Resolved, Resolver};
pub use target::Target;

/// The environment variable `vamd` reads the mirror list from.
pub const ENV_PACKAGE_MIRRORS: &str = "VAMD_PACKAGE_MIRRORS";

/// In the cache key; bump on any unpack or layout change.
pub const FETCHER_VERSION: u32 = 1;

/// Which package world a lockfile belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ecosystem {
    Python,
    Js,
}

impl Ecosystem {
    pub fn as_str(&self) -> &'static str {
        match self {
            Ecosystem::Python => "python",
            Ecosystem::Js => "js",
        }
    }
}

/// Where a resolved tree came from (record field `packages.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Mirror,
    Cache,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Mirror => "mirror",
            Source::Cache => "cache",
        }
    }
}

/// One ecosystem's plan: what the lockfile asks for, and its cache key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub ecosystem: Ecosystem,
    /// 64 hex characters.
    pub key: String,
    pub package_count: u32,
}

/// Why dependencies could not be read or fetched.
#[derive(Debug, thiserror::Error)]
pub enum DepsError {
    /// The author's input is unacceptable. User-facing text; W2 maps it to `InvalidContract`.
    #[error("{0}")]
    Refused(String),
    /// A mirror is down, DNS failed, a timeout or a full disk. Retryable.
    #[error("{0}")]
    Unavailable(String),
    /// A local cache fault.
    #[error("skill dependencies: local cache fault: {0}")]
    Io(#[from] std::io::Error),
}

impl DepsError {
    /// True when the author must change something (as opposed to a retryable or local fault).
    pub fn is_refusal(&self) -> bool {
        matches!(self, DepsError::Refused(_))
    }
}

/// Every user-facing text starts with this.
const PREFIX: &str = "skill dependencies: ";

pub(crate) fn refused(text: impl AsRef<str>) -> DepsError {
    DepsError::Refused(format!("{PREFIX}{}", text.as_ref()))
}

pub(crate) fn unavailable(text: impl AsRef<str>) -> DepsError {
    DepsError::Unavailable(format!("{PREFIX}{}", text.as_ref()))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    fn src(name: &str) -> String {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/skill_deps")
            .join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
    }

    /// The part of a source file before its `#[cfg(test)]` module.
    fn production(name: &str) -> String {
        let s = src(name);
        match s.find("#[cfg(test)]") {
            Some(i) => s[..i].to_string(),
            None => s,
        }
    }

    #[test]
    fn pure_modules_do_no_io() {
        for f in [
            "names.rs",
            "marker.rs",
            "wheel_tags.rs",
            "requirements.rs",
            "pylock.rs",
            "npm.rs",
            "plan.rs",
            "target.rs",
            "limits.rs",
        ] {
            let body = production(f);
            for forbidden in [
                "std::fs",
                "std::net",
                "reqwest",
                "std::env",
                "std::process",
                "tempfile",
                "tokio",
            ] {
                assert!(
                    !body.contains(forbidden),
                    "{f} must stay pure but mentions {forbidden}"
                );
            }
        }
    }

    #[test]
    fn no_subprocess_anywhere_in_the_module() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/skill_deps");
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let name = entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned();
            if !name.ends_with(".rs") {
                continue;
            }
            let body = production(&name);
            for forbidden in [
                "process::Command",
                "Command::new",
                "tokio::process",
                "libc::system",
                "libc::exec",
                "libc::fork",
                "libc::posix_spawn",
            ] {
                assert!(!body.contains(forbidden), "{name} mentions {forbidden}");
            }
        }
    }

    #[test]
    fn mirrors_parse_has_no_relaxing_parameter_and_no_env_switch() {
        let m = production("mirrors.rs");
        assert!(
            m.contains("pub fn parse(value: Option<&str>) -> Result<Mirrors, DepsError>"),
            "Mirrors::parse takes only the value"
        );
        for f in [
            "mirrors.rs",
            "transport.rs",
            "index.rs",
            "resolve.rs",
            "cache.rs",
            "unpack.rs",
        ] {
            let body = production(f);
            assert!(!body.contains("std::env"), "{f} reads the environment");
            assert!(!body.contains("danger_accept"), "{f} relaxes TLS");
        }
        // The loopback seam exists only in test builds.
        let t = production("transport.rs");
        assert!(
            !t.contains("fn for_tests"),
            "for_tests must be inside the #[cfg(test)] part"
        );
    }
}
