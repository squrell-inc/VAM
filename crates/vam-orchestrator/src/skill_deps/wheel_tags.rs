//! Wheel filenames and tag choice for the target: python tag `py3`, `py3N` or `cp3<minor>`, abi
//! `none`, `abi3` or `cp3<minor>`, platform `any` or manylinux x86_64 up to the glibc. Pure.

use super::target::Target;

/// `{name}-{version}[-{build}]-{python}-{abi}-{platform}.whl`, each tag field possibly
/// dot-compressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WheelName {
    pub name: String,
    pub version: String,
    pub python: Vec<String>,
    pub abi: Vec<String>,
    pub platform: Vec<String>,
}

impl WheelName {
    pub(crate) fn parse(filename: &str) -> Option<WheelName> {
        let stem = filename.strip_suffix(".whl")?;
        let parts: Vec<&str> = stem.split('-').collect();
        if parts.len() != 5 && parts.len() != 6 {
            return None;
        }
        if parts.iter().any(|p| p.is_empty()) {
            return None;
        }
        let n = parts.len();
        let split = |s: &str| s.split('.').map(str::to_string).collect::<Vec<_>>();
        Some(WheelName {
            name: parts[0].to_string(),
            version: parts[1].to_string(),
            python: split(parts[n - 3]),
            abi: split(parts[n - 2]),
            platform: split(parts[n - 1]),
        })
    }

    /// Every `python-abi-platform` triple, for messages.
    pub(crate) fn tags(&self) -> String {
        format!(
            "{}-{}-{}",
            self.python.join("."),
            self.abi.join("."),
            self.platform.join(".")
        )
    }

    /// The best compatible rank, or `None` when no tag combination fits the target.
    /// Higher is better: abi-specific first, then abi3, then `none`; then the manylinux level.
    pub(crate) fn rank(&self, target: &Target) -> Option<(u8, u32)> {
        let mut best: Option<(u8, u32)> = None;
        for py in &self.python {
            for abi in &self.abi {
                let Some(kind) = abi_kind(py, abi, target.python_minor) else {
                    continue;
                };
                for plat in &self.platform {
                    let Some(level) = platform_level(plat, target.glibc_minor) else {
                        continue;
                    };
                    let cand = (kind, level);
                    if best.is_none_or(|b| cand > b) {
                        best = Some(cand);
                    }
                }
            }
        }
        best
    }
}

/// `cp3N` gives N; anything else `None` (so `cp313t` is refused).
fn cp_minor(tag: &str) -> Option<u32> {
    let digits = tag.strip_prefix("cp3")?;
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn abi_kind(py: &str, abi: &str, minor: u32) -> Option<u8> {
    match abi {
        "none" => {
            let ok = py == "py3"
                || py
                    .strip_prefix("py3")
                    .and_then(|n| n.parse::<u32>().ok())
                    .is_some_and(|n| n <= minor)
                || cp_minor(py) == Some(minor);
            ok.then_some(1)
        }
        "abi3" => cp_minor(py).filter(|n| *n <= minor).map(|_| 2),
        _ => {
            let abi_minor = cp_minor(abi)?;
            (abi_minor == minor && cp_minor(py) == Some(minor)).then_some(3)
        }
    }
}

/// `any` is level 0; manylinux1, 2010 and 2014 are glibc 2.5, 2.12 and 2.17; `manylinux_2_N`
/// is N. All must be x86_64 and at most the target's glibc.
fn platform_level(plat: &str, glibc_minor: u32) -> Option<u32> {
    if plat == "any" {
        return Some(0);
    }
    let level = match plat {
        "manylinux1_x86_64" => 5,
        "manylinux2010_x86_64" => 12,
        "manylinux2014_x86_64" => 17,
        other => {
            let rest = other.strip_prefix("manylinux_2_")?;
            let n = rest.strip_suffix("_x86_64")?;
            n.parse::<u32>().ok()?
        }
    };
    (level <= glibc_minor).then_some(level)
}

/// Index of the best compatible wheel among `names` (`None` when none fits). Ties are broken by
/// filename, so the choice is deterministic.
pub(crate) fn choose<'a>(
    candidates: &'a [(String, WheelName)],
    target: &Target,
) -> Option<&'a (String, WheelName)> {
    candidates
        .iter()
        .filter_map(|c| c.1.rank(target).map(|r| (r, c)))
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1 .0.cmp(&a.1 .0)))
        .map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Target {
        Target::new(13, 41)
    }

    fn rank(f: &str) -> Option<(u8, u32)> {
        WheelName::parse(f).and_then(|w| w.rank(&t()))
    }

    #[test]
    fn accepted() {
        assert!(rank("vamfixture_hello-1.0.0-py3-none-any.whl").is_some());
        assert!(rank("x-1-py2.py3-none-any.whl").is_some());
        assert!(rank("x-1-cp313-cp313-manylinux_2_28_x86_64.whl").is_some());
        assert!(rank("x-1-cp313-cp313-manylinux_2_17_x86_64.manylinux2014_x86_64.whl").is_some());
        assert!(rank("x-1-cp39-abi3-manylinux_2_28_x86_64.whl").is_some());
        assert!(rank("x-1-cp313-abi3-manylinux2014_x86_64.whl").is_some());
        assert!(rank("x-1-py313-none-any.whl").is_some());
        assert!(rank("x-1-1-py3-none-any.whl").is_some(), "build tag");
        assert!(rank("x-1-cp313-cp313-manylinux_2_41_x86_64.whl").is_some());
        assert!(rank("x-1-py3-none-manylinux1_x86_64.whl").is_some());
    }

    #[test]
    fn refused() {
        for f in [
            "x-1-cp313-cp313-manylinux_2_42_x86_64.whl",
            "x-1-cp313-cp313-linux_x86_64.whl",
            "x-1-cp313-cp313-musllinux_1_2_x86_64.whl",
            "x-1-cp313-cp313-manylinux_2_28_aarch64.whl",
            "x-1-cp313-cp313-macosx_11_0_arm64.whl",
            "x-1-cp313-cp313-win_amd64.whl",
            "x-1-cp313-cp313t-manylinux_2_28_x86_64.whl",
            "x-1-cp313t-cp313t-manylinux_2_28_x86_64.whl",
            "x-1-cp312-cp312-manylinux_2_28_x86_64.whl",
            "x-1-cp314-abi3-manylinux_2_28_x86_64.whl",
            "x-1-py314-none-any.whl",
            "x-1-py2-none-any.whl",
            "x-1-pp313-pypy313_pp73-manylinux_2_28_x86_64.whl",
            "x-1-py3-none-manylinux_2_42_x86_64.whl",
        ] {
            assert_eq!(rank(f), None, "{f}");
        }
    }

    #[test]
    fn unparseable_names() {
        for f in [
            "x-1.whl",
            "x-1-py3-none.whl",
            "notawheel.tar.gz",
            "x--py3-none-any.whl",
            "a-b-c-d-e-f-g.whl",
        ] {
            assert_eq!(WheelName::parse(f), None, "{f}");
        }
    }

    #[test]
    fn ranking_prefers_specific_abi_then_abi3_then_none_then_higher_manylinux() {
        let c = rank("x-1-cp313-cp313-manylinux_2_17_x86_64.whl");
        let a3 = rank("x-1-cp39-abi3-manylinux_2_28_x86_64.whl");
        let pure = rank("x-1-py3-none-any.whl");
        let hi = rank("x-1-cp313-cp313-manylinux_2_28_x86_64.whl");
        assert!(c > a3 && a3 > pure);
        assert!(hi > c, "higher manylinux first within one abi kind");
    }

    #[test]
    fn choice_is_deterministic_and_ties_break_by_filename() {
        let mk = |f: &str| (f.to_string(), WheelName::parse(f).expect("parse"));
        let a = vec![
            mk("x-1-py3-none-any.whl"),
            mk("x-1-cp313-cp313-manylinux_2_28_x86_64.whl"),
            mk("x-1-cp39-abi3-manylinux_2_28_x86_64.whl"),
        ];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(
            choose(&a, &t()).map(|c| c.0.as_str()),
            Some("x-1-cp313-cp313-manylinux_2_28_x86_64.whl")
        );
        assert_eq!(choose(&a, &t()), choose(&b, &t()));
        let tie = vec![mk("x-1-py3-none-any.whl"), mk("x-1-py313-none-any.whl")];
        // py3-none-any and py313-none-any rank equally; the smaller filename wins.
        assert_eq!(
            choose(&tie, &t()).map(|c| c.0.as_str()),
            Some("x-1-py3-none-any.whl")
        );
        let mut rev = tie.clone();
        rev.reverse();
        assert_eq!(
            choose(&rev, &t()).map(|c| c.0.as_str()),
            Some("x-1-py3-none-any.whl")
        );
    }

    #[test]
    fn glibc_bound_moves_with_target() {
        let w = WheelName::parse("x-1-cp313-cp313-manylinux_2_35_x86_64.whl").expect("parse");
        assert!(w.rank(&Target::new(13, 41)).is_some());
        assert!(w.rank(&Target::new(13, 31)).is_none());
        assert!(
            w.rank(&Target::new(12, 41)).is_none(),
            "abi-specific wheel needs the minor"
        );
    }
}
