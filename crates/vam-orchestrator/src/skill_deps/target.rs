//! The platform the packages must run on: the default image's python minor, glibc and arch,
//! and the node platform. Pure.

use super::Ecosystem;
use std::collections::BTreeMap;

/// Default python of `vam/base:2` when the manifest says nothing usable.
const DEFAULT_PYTHON: (u32, u32) = (13, 5);
/// Default glibc minor (2.41) of `vam/base:2`.
const DEFAULT_GLIBC_MINOR: u32 = 41;

/// What the packages are fetched for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub(crate) python_minor: u32,
    pub(crate) python_patch: u32,
    pub(crate) glibc_minor: u32,
}

impl Target {
    /// Python minor and glibc minor, patch 0; used by tests and as a building block.
    pub fn new(python_minor: u32, glibc_minor: u32) -> Target {
        Target {
            python_minor,
            python_patch: 0,
            glibc_minor,
        }
    }

    /// From an image manifest's runtimes: `python` "3.13.5", optional `glibc` "2.41" (default 2.41).
    /// A missing or unreadable python version falls back to the default image's.
    pub fn from_runtimes(r: &BTreeMap<String, String>) -> Target {
        let (python_minor, python_patch) = r
            .get("python")
            .and_then(|v| parse_python(v))
            .unwrap_or(DEFAULT_PYTHON);
        let glibc_minor = r
            .get("glibc")
            .and_then(|v| parse_glibc(v))
            .unwrap_or(DEFAULT_GLIBC_MINOR);
        Target {
            python_minor,
            python_patch,
            glibc_minor,
        }
    }

    /// Every platform fact in one string (for `deps.json` and messages).
    pub fn abi_string(&self) -> String {
        format!(
            "{};{}",
            self.abi_for(Ecosystem::Python),
            self.abi_for(Ecosystem::Js)
        )
    }

    /// The facts one ecosystem's packages depend on. The python patch is in the key because a
    /// `python_full_version` marker can depend on it.
    pub(crate) fn abi_for(&self, eco: Ecosystem) -> String {
        match eco {
            Ecosystem::Python => format!(
                "python3.{}.{}-glibc2.{}-x86_64",
                self.python_minor, self.python_patch, self.glibc_minor
            ),
            Ecosystem::Js => "node-linux-x64".to_string(),
        }
    }

    pub(crate) fn python_full_version(&self) -> String {
        format!("3.{}.{}", self.python_minor, self.python_patch)
    }

    pub(crate) fn python_version(&self) -> String {
        format!("3.{}", self.python_minor)
    }
}

fn parse_python(v: &str) -> Option<(u32, u32)> {
    let mut it = v.trim().split('.');
    let major: u32 = it.next()?.parse().ok()?;
    let minor: u32 = it.next()?.parse().ok()?;
    let patch: u32 = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major == 3).then_some((minor, patch))
}

fn parse_glibc(v: &str) -> Option<u32> {
    let mut it = v.trim().split('.');
    let major: u32 = it.next()?.parse().ok()?;
    let minor: u32 = it.next()?.parse().ok()?;
    (major == 2).then_some(minor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn runtimes_read_python_and_glibc() {
        let t = Target::from_runtimes(&rt(&[("python", "3.12.4"), ("glibc", "2.39")]));
        assert_eq!((t.python_minor, t.python_patch, t.glibc_minor), (12, 4, 39));
    }

    #[test]
    fn defaults_when_missing_or_unreadable() {
        for r in [
            rt(&[]),
            rt(&[("python", "banana"), ("glibc", "x")]),
            rt(&[("python", "2.7.18")]),
        ] {
            let t = Target::from_runtimes(&r);
            assert_eq!((t.python_minor, t.glibc_minor), (13, 41));
        }
    }

    #[test]
    fn abi_strings_name_the_platform() {
        let t = Target::new(13, 41);
        assert_eq!(
            t.abi_for(Ecosystem::Python),
            "python3.13.0-glibc2.41-x86_64"
        );
        assert_eq!(t.abi_for(Ecosystem::Js), "node-linux-x64");
        assert!(t.abi_string().contains("node-linux-x64"));
    }
}
