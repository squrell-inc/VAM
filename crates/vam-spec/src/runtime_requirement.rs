//! `SkillManifest.requires_runtime`: the syntax of a requirement and the version compare.
//!
//! Syntax (machine-base open question 9): `""` or `*` (any version), `>=X[.Y[.Z]]`,
//! `=X[.Y[.Z]]`. Nothing else. Checked at install and release against the base the node
//! reports (`vam-orchestrator::node_base`).

use crate::validation::ValidationError;

/// A parsed requirement on one runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeRequirement {
    Any,
    AtLeast(Vec<u64>),
    Exactly(Vec<u64>),
}

/// Why a requirement did not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequirementError {
    #[error("requirement '{0}' is not '*', '>=X[.Y[.Z]]' or '=X[.Y[.Z]]'")]
    Syntax(String),
}

/// Numeric components of `X[.Y[.Z]]` (one to three). Used for requirements.
fn parse_components(s: &str) -> Option<Vec<u64>> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 3 {
        return None;
    }
    parts
        .iter()
        .map(|p| {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                None
            } else {
                p.parse::<u64>().ok()
            }
        })
        .collect()
}

/// The leading numeric components of an installed version such as `3.13.5` or `1.2.0-r1`.
/// `None` when it does not start with a number.
fn installed_components(s: &str) -> Option<Vec<u64>> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let head = s[..end].trim_end_matches('.');
    if head.is_empty() {
        return None;
    }
    head.split('.')
        .map(|p| {
            if p.is_empty() {
                None
            } else {
                p.parse::<u64>().ok()
            }
        })
        .collect()
}

/// Parse one requirement value.
pub fn parse_requirement(s: &str) -> Result<RuntimeRequirement, RequirementError> {
    let t = s.trim();
    if t.is_empty() || t == "*" {
        return Ok(RuntimeRequirement::Any);
    }
    let bad = || RequirementError::Syntax(s.to_string());
    if let Some(rest) = t.strip_prefix(">=") {
        return parse_components(rest.trim())
            .map(RuntimeRequirement::AtLeast)
            .ok_or_else(bad);
    }
    if let Some(rest) = t.strip_prefix('=') {
        return parse_components(rest.trim())
            .map(RuntimeRequirement::Exactly)
            .ok_or_else(bad);
    }
    Err(bad())
}

fn cmp_components(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x.cmp(&y);
        }
    }
    std::cmp::Ordering::Equal
}

impl RuntimeRequirement {
    /// Whether the installed `have` meets this requirement. An installed version that does
    /// not start with a number meets only `Any`. `=X.Y` matches any `X.Y.*` (the requirement
    /// names a prefix); `=X.Y.Z` matches that version.
    pub fn satisfied_by(&self, have: &str) -> bool {
        let Some(h) = installed_components(have) else {
            return matches!(self, RuntimeRequirement::Any);
        };
        match self {
            RuntimeRequirement::Any => true,
            RuntimeRequirement::AtLeast(r) => cmp_components(&h, r) != std::cmp::Ordering::Less,
            RuntimeRequirement::Exactly(r) => {
                let n = r.len();
                let head: Vec<u64> = (0..n).map(|i| h.get(i).copied().unwrap_or(0)).collect();
                head == *r
            }
        }
    }
}

/// Whether `s` is a legal runtime name: `[a-z][a-z0-9-]*`.
pub fn valid_runtime_name(s: &str) -> bool {
    let mut b = s.bytes();
    matches!(b.next(), Some(c) if c.is_ascii_lowercase())
        && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// Validate a manifest's `requires_runtime` map into `errors` at `requires_runtime[<key>]`.
pub fn validate_requires_runtime(
    map: &std::collections::HashMap<String, String>,
    errors: &mut ValidationError,
) {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    for k in keys {
        let field = format!("requires_runtime[{k}]");
        if !valid_runtime_name(k) {
            errors.push(
                &field,
                format!("runtime name '{k}' must match [a-z][a-z0-9-]*"),
                "runtime_name",
            );
        }
        if let Err(e) = parse_requirement(&map[k]) {
            errors.push(&field, e.to_string(), "runtime_requirement");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_three_forms() {
        assert_eq!(parse_requirement("").unwrap(), RuntimeRequirement::Any);
        assert_eq!(parse_requirement("*").unwrap(), RuntimeRequirement::Any);
        assert_eq!(
            parse_requirement(">=3.11").unwrap(),
            RuntimeRequirement::AtLeast(vec![3, 11])
        );
        assert_eq!(
            parse_requirement("=21").unwrap(),
            RuntimeRequirement::Exactly(vec![21])
        );
    }

    #[test]
    fn refuses_everything_else() {
        for bad in [
            ">3",
            "<=3",
            "^3",
            "~3",
            ">=",
            ">=a",
            ">=1.2.3.4",
            "3.11",
            ">=1..2",
            "=1.x",
        ] {
            assert!(parse_requirement(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn compares_numerically() {
        let r = parse_requirement(">=3.11").unwrap();
        assert!(r.satisfied_by("3.13.5"));
        assert!(r.satisfied_by("3.11"));
        assert!(r.satisfied_by("4"));
        assert!(!r.satisfied_by("3.9.18"));
        assert!(!r.satisfied_by("2.99"));
        let r = parse_requirement(">=3.14").unwrap();
        assert!(!r.satisfied_by("3.13.5"));
        let r = parse_requirement(">=1.2.0").unwrap();
        assert!(r.satisfied_by("1.2.0-r1"));
    }

    #[test]
    fn exact_matches_its_prefix() {
        let r = parse_requirement("=3.13").unwrap();
        assert!(r.satisfied_by("3.13.5"));
        assert!(!r.satisfied_by("3.12.9"));
        let r = parse_requirement("=1.2.0").unwrap();
        assert!(r.satisfied_by("1.2.0"));
        assert!(!r.satisfied_by("1.2.1"));
    }

    #[test]
    fn non_numeric_installed_meets_only_any() {
        assert!(parse_requirement("*").unwrap().satisfied_by("unknown"));
        assert!(!parse_requirement(">=1").unwrap().satisfied_by("unknown"));
    }

    #[test]
    fn names() {
        assert!(valid_runtime_name("python"));
        assert!(valid_runtime_name("node-js2"));
        assert!(!valid_runtime_name("Python"));
        assert!(!valid_runtime_name("2py"));
        assert!(!valid_runtime_name(""));
    }
}
