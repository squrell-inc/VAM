//! `vam <kind> suggest-version` and the `--version`-less `release` flow. The
//! server owns the classification (`GET .../release-suggestion`); this module
//! only fetches it, formats it and asks.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::client::{self, encode_path_segment, Client};
use crate::config;
use crate::output::{self, fail, ExitCode};

/// Where the suggestion for a kind lives.
pub(super) fn suggestion_path(collection: &str, urn: &str) -> String {
    let urn = encode_path_segment(urn);
    if collection == "skills" {
        // The CLI releases skill packages, not bare manifests.
        format!("/v1/skills/packages/{urn}/release-suggestion")
    } else {
        format!("/v1/{collection}/{urn}/release-suggestion")
    }
}

fn triple(v: &Value) -> Option<String> {
    if v.is_null() {
        return None;
    }
    let n = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
    Some(format!("{}.{}.{}", n("major"), n("minor"), n("patch")))
}

fn reasons(s: &Value) -> Vec<String> {
    s.get("reasons")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Sentences from `permissionChanges`, in response order. Entries without a
/// sentence are skipped.
fn permission_change_lines(s: &Value) -> Vec<String> {
    s.get("permissionChanges")
        .and_then(Value::as_array)
        .map(|changes| {
            changes
                .iter()
                .filter_map(|change| change.get("sentence").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn print_permission_changes(s: &Value) {
    for line in permission_change_lines(s) {
        println!("{line}");
    }
}

fn bump_word(s: &Value) -> String {
    s.get("bump")
        .and_then(Value::as_str)
        .unwrap_or("PATCH")
        .to_lowercase()
}

/// The suggested version, e.g. `1.1.0`.
pub(super) fn suggested(s: &Value) -> Result<String> {
    s.get("suggested")
        .and_then(triple)
        .context("release suggestion has no suggested version")
}

/// `minor: added skill jira`, `initial: first release`, `none: unchanged`.
pub(super) fn summary(s: &Value) -> String {
    let bump = bump_word(s);
    let reasons = reasons(s);
    if reasons.is_empty() {
        format!("{bump}: unchanged since the latest release")
    } else {
        format!("{bump}: {}", reasons.join("; "))
    }
}

pub(super) async fn fetch(c: &Client, collection: &str, urn: &str) -> Result<Value> {
    let body = c.get(&suggestion_path(collection, urn)).await?;
    serde_json::from_str(&body).context("parse release suggestion")
}

pub async fn suggest_version(c: &Client, collection: &str, urn: &str) -> Result<()> {
    let body = c.get(&suggestion_path(collection, urn)).await?;
    let s: Value = serde_json::from_str(&body).context("parse release suggestion")?;
    if output::is_json() {
        client::print_json(&body);
        return Ok(());
    }
    print_permission_changes(&s);
    println!("suggested: {} ({})", suggested(&s)?, summary(&s));
    if let Some(latest) = s.get("latest").and_then(triple) {
        println!("latest:    {latest}");
    }
    Ok(())
}

/// The version to release: the flag, else the suggestion confirmed on a TTY.
/// Without a TTY (or with `--json`) it fails and names the suggestion, so a
/// script never blocks on a prompt.
pub async fn version_for_release(
    c: &Client,
    collection: &str,
    urn: &str,
    version: Option<String>,
) -> Result<String> {
    if let Some(version) = version {
        return Ok(version);
    }
    let s = fetch(c, collection, urn).await?;
    let next = suggested(&s)?;
    if !output::is_json() {
        print_permission_changes(&s);
    }
    if !output::stdin_is_tty() || output::is_json() {
        return Err(fail(
            ExitCode::Usage,
            format!("--version is required; suggested: {next} ({})", summary(&s)),
        ));
    }
    println!("suggested: {next} ({})", summary(&s));
    if let Some(latest) = s.get("latest").and_then(triple) {
        println!("latest:    {latest}");
    }
    let answer = config::prompt(&format!("Release version [{next}]: "), "--version")?;
    Ok(if answer.is_empty() { next } else { answer })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summary_joins_reasons_and_lowercases_the_bump() {
        let s = json!({"bump": "MAJOR", "reasons": ["removed skill jira", "edited instructions"]});
        assert_eq!(
            summary(&s),
            "major: removed skill jira; edited instructions"
        );
    }

    #[test]
    fn omitted_zero_fields_read_as_zero() {
        let s = json!({"suggested": {"major": 1}});
        assert_eq!(suggested(&s).unwrap(), "1.0.0");
    }

    #[test]
    fn permission_change_lines_are_the_sentences_in_order() {
        let s = json!({
            "permissionChanges": [
                {
                    "sentence": "Now allowed without asking: `mcp:mcp.atlassian.com/create_issue` (was: ask)",
                    "change": "loosened"
                },
                {"target": "tools.default"},
                {"sentence": "Now denied: `*.pastebin.com`"}
            ]
        });
        assert_eq!(
            permission_change_lines(&s),
            vec![
                "Now allowed without asking: `mcp:mcp.atlassian.com/create_issue` (was: ask)",
                "Now denied: `*.pastebin.com`",
            ]
        );
    }

    #[test]
    fn skill_suggestions_use_the_package_route() {
        assert_eq!(
            suggestion_path("skills", "urn:vam:skill:a/b/c"),
            "/v1/skills/packages/urn%3Avam%3Askill%3Aa%2Fb%2Fc/release-suggestion"
        );
        assert!(suggestion_path("agents", "u").starts_with("/v1/agents/"));
    }
}
