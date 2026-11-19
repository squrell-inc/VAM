//! What a binary reports about the build it is: version, commit, date, channel.
//!
//! Git is read only in the binary crates' build scripts (`build_info_logic.rs`); this module is
//! the pure part: the type, its printed line, and `build_info!()`, which reads the values the
//! calling crate's build script exported.

#[allow(dead_code)]
#[path = "../build_info_logic.rs"]
mod logic;

pub use logic::{CHANNEL_DEV, CHANNEL_RELEASE};

/// The build a binary was made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildInfo {
    /// `0.3.0`, `0.3.0-rc.1`, `0.3.0+dev.14` or `0.3.0+dev`.
    pub version: String,
    /// Short SHA, with `+dirty`, or `unknown`.
    pub commit: String,
    /// `YYYY-MM-DD`, or empty.
    pub commit_date: String,
    /// `release` or `dev`.
    pub channel: String,
}

impl BuildInfo {
    pub fn new(version: &str, commit: &str, commit_date: &str, channel: &str) -> Self {
        Self {
            version: version.to_string(),
            commit: commit.to_string(),
            commit_date: commit_date.to_string(),
            channel: channel.to_string(),
        }
    }

    /// `<version> (<commit>[ <date>])`; the date only on release builds.
    pub fn version_line(&self) -> String {
        if self.channel == CHANNEL_RELEASE && !self.commit_date.is_empty() {
            format!("{} ({} {})", self.version, self.commit, self.commit_date)
        } else {
            format!("{} ({})", self.version, self.commit)
        }
    }

    /// `<name> <version> (<commit>[ <date>])`.
    pub fn line(&self, name: &str) -> String {
        format!("{name} {}", self.version_line())
    }

    /// `version_line()` as a `&'static str` (leaked once), for clap's `version`.
    pub fn leaked_version_line(&self) -> &'static str {
        Box::leak(self.version_line().into_boxed_str())
    }

    /// The `major.minor` of `version`, ignoring any `-pre` or `+build` suffix.
    pub fn major_minor(version: &str) -> Option<(u64, u64)> {
        let core = version.split(['-', '+']).next()?;
        let mut it = core.split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next()?.parse().ok()?;
        Some((major, minor))
    }
}

/// The `BuildInfo` of the crate this is expanded in, from the values its `build.rs` exported
/// (`emit_build_info`). Only the five binary crates have that build script.
#[macro_export]
macro_rules! build_info {
    () => {
        $crate::build_info::BuildInfo::new(
            env!("VAM_VERSION"),
            env!("VAM_GIT_SHA"),
            env!("VAM_COMMIT_DATE"),
            env!("VAM_BUILD_CHANNEL"),
        )
    };
}

#[cfg(test)]
mod tests {
    use super::logic::*;
    use super::*;

    #[test]
    fn release_uses_cargo_version_exactly() {
        let c = resolve_channel(Some("release"));
        assert_eq!(c, "release");
        assert_eq!(
            resolve_version(c, "0.3.0", Some("v0.2.0-5-gabc1234")),
            "0.3.0"
        );
    }

    #[test]
    fn other_channel_values_are_dev() {
        assert_eq!(resolve_channel(None), "dev");
        assert_eq!(resolve_channel(Some("Release2")), "dev");
        assert_eq!(resolve_channel(Some("")), "dev");
    }

    #[test]
    fn dev_after_tag_counts_commits() {
        assert_eq!(
            resolve_version("dev", "0.3.0", Some("v0.3.0-14-g9be41d0")),
            "0.3.0+dev.14"
        );
    }

    #[test]
    fn dev_at_the_tag_is_dev_zero() {
        assert_eq!(
            resolve_version("dev", "0.3.0", Some("v0.3.0-0-g9be41d0")),
            "0.3.0+dev.0"
        );
    }

    #[test]
    fn rc_tag_keeps_its_label() {
        assert_eq!(
            resolve_version("dev", "0.1.0", Some("v0.1.0-rc.1-3-gabc1234")),
            "0.1.0-rc.1+dev.3"
        );
    }

    #[test]
    fn no_tag_or_unparsable_describe_falls_back_to_cargo_version() {
        assert_eq!(resolve_version("dev", "0.1.0", None), "0.1.0+dev");
        assert_eq!(
            resolve_version("dev", "0.1.0", Some("garbage")),
            "0.1.0+dev"
        );
        assert_eq!(
            resolve_version("dev", "0.1.0", Some("abc1234")),
            "0.1.0+dev"
        );
    }

    #[test]
    fn commit_rules() {
        assert_eq!(resolve_commit(None, Some("9be41d0"), false), "9be41d0");
        assert_eq!(resolve_commit(None, Some("9be41d0"), true), "9be41d0+dirty");
        assert_eq!(resolve_commit(None, None, false), "unknown");
        assert_eq!(
            resolve_commit(Some("3f9c2ab"), Some("9be41d0"), true),
            "3f9c2ab"
        );
        assert_eq!(resolve_commit(Some(" "), Some("9be41d0"), false), "9be41d0");
    }

    #[test]
    fn date_rules() {
        assert_eq!(resolve_date(None, Some("2026-11-20")), "2026-11-20");
        assert_eq!(
            resolve_date(Some("2026-01-02"), Some("2026-11-20")),
            "2026-01-02"
        );
        assert_eq!(resolve_date(None, None), "");
    }

    #[test]
    fn lines_show_the_date_on_release_only() {
        let dev = BuildInfo::new("0.3.0+dev.14", "9be41d0", "2026-11-20", "dev");
        assert_eq!(dev.line("vam"), "vam 0.3.0+dev.14 (9be41d0)");
        let rel = BuildInfo::new("0.3.0", "3f9c2ab", "2026-11-20", "release");
        assert_eq!(rel.line("vam"), "vam 0.3.0 (3f9c2ab 2026-11-20)");
        let nodate = BuildInfo::new("0.3.0+dev", "unknown", "", "dev");
        assert_eq!(nodate.line("vamd"), "vamd 0.3.0+dev (unknown)");
    }

    #[test]
    fn major_minor_ignores_suffixes() {
        assert_eq!(BuildInfo::major_minor("0.3.0+dev.14"), Some((0, 3)));
        assert_eq!(BuildInfo::major_minor("0.1.0-rc.1"), Some((0, 1)));
        assert_eq!(BuildInfo::major_minor("x"), None);
    }
}
