//! `vam catalog validate <dir>`: the package checks of `vam-spec`, offline. No vamd, no token.
//!
//! `<dir>` is either one artifact (a skill, agent or playbook directory) or a marketplace root
//! (a directory holding `marketplace.json`: every listing's artifacts are checked). The checks
//! are the ones vamd runs at publish and at marketplace import, so a pass here means a pass
//! there, apart from node policy (which registry domains a node admits).
//!
//! Output is one line per finding, then one `ok` line per artifact that passed:
//!
//! ```text
//! error   microsoft-365 listings/microsoft-365/skills/m365-mail: <message>
//! warning microsoft-365 listings/microsoft-365/playbooks/morning-triage: <message>
//! ok      skill urn:vam:skill:hub.squrell.cloud/squrell/m365-mail
//! ```
//!
//! Exit code 0 when there is no error (and no warning with `--strict`), 6 (`invalid`) otherwise.

use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};
use vam_spec::marketplace::{
    check_marketplace_root, Finding, Level, MarketplaceReport, INDEX_FILE,
};
use vam_spec::package::{check_dir, ArtifactKind};

use crate::output::{self, fail, ExitCode};

/// Check `dir`. Returns the report on success; the error carries exit code 6 (`invalid`).
pub fn run(dir: &str, strict: bool, kind: Option<&str>) -> Result<()> {
    let path = Path::new(dir);
    let report = if path.join(INDEX_FILE).exists() {
        if kind.is_some() {
            return Err(fail(
                ExitCode::Usage,
                "--kind applies to one artifact directory, not a marketplace root",
            ));
        }
        check_marketplace_root(path)
    } else {
        check_one(path, kind)?
    };
    present(&report, strict)
}

fn check_one(path: &Path, kind: Option<&str>) -> Result<MarketplaceReport> {
    let expect = match kind {
        None => None,
        Some(word) => Some(ArtifactKind::parse(word).ok_or_else(|| {
            fail(
                ExitCode::Usage,
                format!("--kind must be skill, agent or playbook, got '{word}'"),
            )
        })?),
    };
    let mut report = MarketplaceReport::default();
    match check_dir(path, expect) {
        Ok(checked) => {
            for w in &checked.warnings {
                report
                    .findings
                    .push(Finding::warning(None, None, w.clone()));
            }
            report
                .artifacts
                .push(vam_spec::marketplace::CheckedArtifact {
                    listing: String::new(),
                    path: path.display().to_string(),
                    kind: checked.kind,
                    urn: checked.urn,
                    summary: checked.summary,
                });
        }
        Err(e) => report.findings.push(Finding::error(None, None, e.0)),
    }
    Ok(report)
}

fn finding_line(f: &Finding) -> String {
    let level = match f.level {
        Level::Error => "error  ",
        Level::Warning => "warning",
    };
    let place = [f.listing.as_deref(), f.path.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    // One line per finding: a message that spans lines is joined.
    let message = f.message.split_whitespace().collect::<Vec<_>>().join(" ");
    if place.is_empty() {
        format!("{level} {message}")
    } else {
        format!("{level} {place}: {message}")
    }
}

fn present(report: &MarketplaceReport, strict: bool) -> Result<()> {
    let errors = report.errors();
    let warnings = report.warnings();
    let failed = errors > 0 || (strict && warnings > 0);
    let summary = format!(
        "{} artifact(s) ok, {errors} error(s), {warnings} warning(s)",
        report.artifacts.len()
    );
    if output::is_json() {
        if failed {
            let lines: Vec<String> = report.findings.iter().map(finding_line).collect();
            return Err(fail(
                ExitCode::Invalid,
                format!("{summary}; {}", lines.join("; ")),
            ));
        }
        let doc: Value = json!({
            "ok": true,
            "errors": errors,
            "warnings": warnings,
            "findings": report.findings,
            "artifacts": report.artifacts,
        });
        println!("{doc}");
        return Ok(());
    }
    for f in &report.findings {
        println!("{}", finding_line(f));
    }
    for a in &report.artifacts {
        println!("ok      {} {}", a.kind.as_str(), a.urn);
    }
    println!("{summary}");
    if failed {
        let why = if errors > 0 {
            format!("{errors} error(s)")
        } else {
            format!("{warnings} warning(s) with --strict")
        };
        return Err(fail(ExitCode::Invalid, format!("validation failed: {why}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e/world/fixtures");
        root.join(name).display().to_string()
    }

    #[test]
    fn a_good_marketplace_root_passes() {
        run(&fixture("marketplace-squrell"), false, None).unwrap();
    }

    #[test]
    fn strict_fails_on_a_warning_and_a_broken_index_is_invalid() {
        let e = run(&fixture("marketplace-community"), true, None).unwrap_err();
        assert_eq!(output::classify(&e).0, ExitCode::Invalid);
        let e = run(&fixture("marketplace-community-broken"), false, None).unwrap_err();
        assert_eq!(output::classify(&e).0, ExitCode::Invalid);
    }

    #[test]
    fn one_artifact_directory_and_the_kind_flag() {
        let dir = fixture("marketplace-squrell/listings/microsoft-365/skills/m365-mail");
        run(&dir, false, Some("skill")).unwrap();
        run(&dir, false, None).unwrap();
        let e = run(&dir, false, Some("agent")).unwrap_err();
        assert_eq!(output::classify(&e).0, ExitCode::Invalid);
        let e = run(&dir, false, Some("widget")).unwrap_err();
        assert_eq!(output::classify(&e).0, ExitCode::Usage);
    }
}
