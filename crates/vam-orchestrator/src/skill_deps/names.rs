//! Package names and versions: PEP 503 normalisation, release-segment version comparison and
//! the npm name and version rules. Pure.

use std::cmp::Ordering;

/// PEP 503: lowercase, every run of `-`, `_` and `.` becomes one `-`.
pub(crate) fn normalize_project(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_sep = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !last_sep {
                out.push('-');
            }
            last_sep = true;
        } else {
            out.push(c.to_ascii_lowercase());
            last_sep = false;
        }
    }
    out
}

/// A name as PEP 508 writes it: `[A-Za-z0-9]` first and last, `[A-Za-z0-9._-]` between.
pub(crate) fn valid_python_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > 214 {
        return false;
    }
    let edge = |c: u8| c.is_ascii_alphanumeric();
    edge(b[0])
        && edge(b[b.len() - 1])
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// A version as `release[suffix]`: digits joined by dots, then whatever follows (`rc1`, `.post2`).
/// `None` when it does not start with a digit.
pub(crate) fn split_version(v: &str) -> Option<(Vec<u64>, String)> {
    let v = v.trim();
    let v = v.strip_prefix(['v', 'V']).unwrap_or(v);
    let mut release = Vec::new();
    let mut rest = v;
    loop {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            break;
        }
        release.push(digits.parse::<u64>().ok()?);
        rest = &rest[digits.len()..];
        match rest.strip_prefix('.') {
            Some(next) if next.starts_with(|c: char| c.is_ascii_digit()) => rest = next,
            _ => break,
        }
    }
    if release.is_empty() {
        return None;
    }
    Some((release, rest.to_ascii_lowercase()))
}

/// Release segments only (`3.13`, `3.13.5`); `None` for anything with a suffix.
pub(crate) fn parse_release(v: &str) -> Option<Vec<u64>> {
    let (rel, suffix) = split_version(v)?;
    suffix.is_empty().then_some(rel)
}

pub(crate) fn cmp_release(a: &[u64], b: &[u64]) -> Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// Two version strings name the same release (`1.0` equals `1.0.0`; suffixes must match).
pub(crate) fn versions_equal(a: &str, b: &str) -> bool {
    match (split_version(a), split_version(b)) {
        (Some((ra, sa)), Some((rb, sb))) => cmp_release(&ra, &rb) == Ordering::Equal && sa == sb,
        _ => a == b,
    }
}

/// An npm package name, optionally scoped: lowercase, URL-safe, at most 214 bytes, not starting
/// with `.` or `_`, no `..`, no `%`.
pub(crate) fn valid_npm_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 214 || name.contains("..") || name.contains('%') {
        return false;
    }
    let part_ok = |p: &str| {
        !p.is_empty()
            && !p.starts_with(['.', '_'])
            && p.bytes().all(|c| {
                c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || matches!(c, b'-' | b'.' | b'_' | b'~')
            })
    };
    match name.strip_prefix('@') {
        Some(scoped) => match scoped.split_once('/') {
            Some((scope, pkg)) => part_ok(scope) && part_ok(pkg),
            None => false,
        },
        None => part_ok(name),
    }
}

/// The last path segment of an npm name (`@scope/name` gives `name`).
pub(crate) fn npm_base_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// Plain semver: `MAJOR.MINOR.PATCH` with optional `-prerelease` and `+build`; no range, tag or URL.
pub(crate) fn is_plain_semver(v: &str) -> bool {
    let (core_pre, build) = match v.split_once('+') {
        Some((a, b)) => (a, Some(b)),
        None => (v, None),
    };
    let (core, pre) = match core_pre.split_once('-') {
        Some((a, b)) => (a, Some(b)),
        None => (core_pre, None),
    };
    let nums: Vec<&str> = core.split('.').collect();
    if nums.len() != 3
        || nums
            .iter()
            .any(|n| n.is_empty() || !n.bytes().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    let ident_ok = |s: &str| {
        !s.is_empty()
            && s.split('.')
                .all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'))
    };
    pre.is_none_or(ident_ok) && build.is_none_or(ident_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pep503_table() {
        for (raw, want) in [
            ("Foo_Bar", "foo-bar"),
            ("foo.bar", "foo-bar"),
            ("foo--bar__baz", "foo-bar-baz"),
            ("Foo-.-Bar", "foo-bar"),
            ("vamfixture-hello", "vamfixture-hello"),
        ] {
            assert_eq!(normalize_project(raw), want, "{raw}");
        }
    }

    #[test]
    fn python_name_table() {
        for ok in ["a", "Foo.Bar-baz_1", "x9"] {
            assert!(valid_python_name(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", ".a", "a b", "a/b", "a@b"] {
            assert!(!valid_python_name(bad), "{bad}");
        }
    }

    #[test]
    fn version_compare() {
        assert!(versions_equal("1.0", "1.0.0"));
        assert!(versions_equal("1.0.0", "1.0.0"));
        assert!(!versions_equal("1.0.0", "1.0.1"));
        assert!(!versions_equal("1.0rc1", "1.0"));
        assert_eq!(
            cmp_release(&[3, 13], &[3, 9]),
            Ordering::Greater,
            "numeric, not lexical"
        );
        assert_eq!(cmp_release(&[3, 13], &[3, 13, 0]), Ordering::Equal);
        assert_eq!(parse_release("3.13.5"), Some(vec![3, 13, 5]));
        assert_eq!(parse_release("3.13rc1"), None);
        assert_eq!(parse_release("linux"), None);
    }

    #[test]
    fn npm_name_table() {
        for ok in ["left-pad", "@scope/name", "a.b_c~d", "@a/b"] {
            assert!(valid_npm_name(ok), "{ok}");
        }
        let long = "a".repeat(215);
        for bad in [
            "",
            "Upper",
            ".hidden",
            "_private",
            "a..b",
            "a%2eb",
            "@scope",
            "@/x",
            "@scope/",
            "a/b",
            "sp ace",
            "@scope/.x",
            "a\0b",
            long.as_str(),
        ] {
            assert!(!valid_npm_name(bad), "{bad:?}");
        }
        assert_eq!(npm_base_name("@scope/name"), "name");
        assert_eq!(npm_base_name("plain"), "plain");
    }

    #[test]
    fn semver_table() {
        for ok in ["1.0.0", "0.0.1-rc.1", "10.20.30+build.5", "1.0.0-alpha-1"] {
            assert!(is_plain_semver(ok), "{ok}");
        }
        for bad in [
            "1.0",
            "^1.0.0",
            "latest",
            "1.0.0 - 2.0.0",
            "file:../x",
            "1.x.0",
            "",
            "1.0.0/../x",
        ] {
            assert!(!is_plain_semver(bad), "{bad}");
        }
    }
}
