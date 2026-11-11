//! `vam agent stats <agent>` and the release preface of `vam agent release`. The server owns
//! the numbers (`GET /v1/agents/{urn}/outcomes[/diff]`); this module fetches and formats.

use anyhow::{bail, Result};
use serde_json::Value;

use crate::client::{self, encode_path_segment, Client};

fn query(version: Option<&str>, k: Option<u32>) -> String {
    let mut q = Vec::new();
    if let Some(v) = version {
        q.push(format!("version={}", encode_path_segment(v)));
    }
    if let Some(k) = k {
        q.push(format!("k={k}"));
    }
    if q.is_empty() {
        String::new()
    } else {
        format!("?{}", q.join("&"))
    }
}

pub(super) async fn run(
    c: &Client,
    agent: &str,
    version: Option<&str>,
    k: Option<u32>,
    diff: bool,
    json: bool,
) -> Result<()> {
    let urn = encode_path_segment(agent);
    if diff {
        let v = version.unwrap_or("draft");
        let body = c
            .get(&format!(
                "/v1/agents/{urn}/outcomes/diff{}",
                query(Some(v), k)
            ))
            .await?;
        if json || crate::output::is_json() {
            client::print_json(&body);
        } else {
            print!("{}", render_diff(&serde_json::from_str(&body)?));
        }
        return Ok(());
    }
    let body = c
        .get(&format!("/v1/agents/{urn}/outcomes{}", query(version, k)))
        .await?;
    if json || crate::output::is_json() {
        client::print_json(&body);
        return Ok(());
    }
    let parsed: Value = serde_json::from_str(&body)?;
    if version.is_none() {
        print!("{}", render_list(&parsed));
    } else {
        print!("{}", render_aggregate(&parsed));
    }
    Ok(())
}

/// The draft-vs-previous diff for `vam agent release`, as text. `Err` is only for the
/// caller to warn about; a release never waits on it.
pub(super) async fn release_preface(c: &Client, agent: &str) -> Result<String> {
    let urn = encode_path_segment(agent);
    let body = c
        .get(&format!("/v1/agents/{urn}/outcomes/diff?version=draft"))
        .await?;
    let parsed: Value = serde_json::from_str(&body)?;
    if parsed.get("candidate").is_none() {
        bail!("the diff response has no candidate");
    }
    Ok(render_diff(&parsed))
}

fn num(v: &Value) -> String {
    match v.as_f64() {
        Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
        Some(f) => format!("{f:.3}"),
        None => "-".into(),
    }
}

fn line(label: &str, value: String) -> String {
    format!("  {label:<22}{value}\n")
}

fn render_aggregate(a: &Value) -> String {
    let mut out = format!("version {}\n", a["version"].as_str().unwrap_or("-"));
    out += &line(
        "runs",
        format!(
            "{} ({} succeeded, {} failed, {} timed out)",
            num(&a["runs"]),
            num(&a["successes"]),
            num(&a["failed"]),
            num(&a["timedOut"])
        ),
    );
    for (key, label) in [
        ("successRate", "success rate"),
        ("approvalsPerRun", "approvals per run"),
        ("tokensPerSuccess", "tokens per success"),
        ("turnsPerSuccess", "turns per success"),
    ] {
        if a.get(key).is_some() {
            out += &line(label, num(&a[key]));
        }
    }
    match a["passK"].get("value") {
        Some(v) => out += &line(&format!("pass^{}", num(&a["passK"]["k"])), num(v)),
        None => {
            if let Some(n) = a["passK"]["note"].as_str() {
                out += &line(
                    &format!("pass^{}", num(&a["passK"]["k"])),
                    format!("- ({n})"),
                );
            }
        }
    }
    if a.get("costPerSuccessMicroUsd").is_some() {
        out += &line(
            "cost per success",
            format!("{} micro-USD", num(&a["costPerSuccessMicroUsd"])),
        );
    } else if let Some(m) = a["costMissing"].as_array() {
        let names: Vec<&str> = m.iter().filter_map(Value::as_str).collect();
        out += &line(
            "cost per success",
            format!("- (no price: {})", names.join(", ")),
        );
    }
    if let Some(e) = a.get("effects") {
        let mut s = format!("{} journaled", num(&e["journaledRuns"]));
        if e.get("duplicates").is_some() {
            s += &format!(
                ", {} confirmed, {} unknown, {} duplicates",
                num(&e["confirmed"]),
                num(&e["unknown"]),
                num(&e["duplicates"])
            );
        }
        out += &line("effects", s);
    }
    out
}

fn render_list(v: &Value) -> String {
    let rows = v["versions"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return "no runs with a receipt yet\n".into();
    }
    let mut out = String::new();
    for r in rows {
        out += &format!(
            "{:<12}{} runs, {} succeeded\n",
            r["version"].as_str().unwrap_or("-"),
            num(&r["runs"]),
            num(&r["successes"])
        );
    }
    out
}

fn render_diff(d: &Value) -> String {
    let cand = &d["candidate"];
    let cv = cand["version"].as_str().unwrap_or("-");
    let Some(base) = d.get("base") else {
        return format!(
            "{cv}: no earlier release to compare with\n{}",
            render_aggregate(cand)
        );
    };
    let mut out = format!(
        "{cv} vs {} ({} runs vs {} runs)\n",
        base["version"].as_str().unwrap_or("-"),
        num(&cand["runs"]),
        num(&base["runs"])
    );
    let ch = &d["changes"];
    let mut any = false;
    for (key, label) in [
        ("successRate", "success rate"),
        ("passK", "pass^k"),
        ("costPerSuccessMicroUsd", "cost per success"),
        ("tokensPerSuccess", "tokens per success"),
        ("approvalsPerRun", "approvals per run"),
    ] {
        if let Some(c) = ch.get(key) {
            any = true;
            out += &line(
                label,
                format!(
                    "{} -> {} ({}{})",
                    num(&c["base"]),
                    num(&c["candidate"]),
                    if c["delta"].as_f64().unwrap_or(0.0) > 0.0 {
                        "+"
                    } else {
                        ""
                    },
                    num(&c["delta"])
                ),
            );
        }
    }
    if let Some(c) = ch.get("duplicates") {
        any = true;
        out += &line(
            "duplicates",
            format!("{} -> {}", num(&c["base"]), num(&c["candidate"])),
        );
    }
    if !any {
        out += "  nothing comparable yet (a side has no runs or no value)\n";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diff_text_lists_the_changes() {
        let d = json!({
            "base": {"version": "1.0.0", "runs": 4},
            "candidate": {"version": "draft", "runs": 2},
            "changes": {
                "successRate": {"base": 0.5, "candidate": 1.0, "delta": 0.5},
                "duplicates": {"base": 1, "candidate": 0, "delta": -1}
            }
        });
        assert_eq!(
            render_diff(&d),
            "draft vs 1.0.0 (2 runs vs 4 runs)\n  success rate          0.500 -> 1 (+0.500)\n  duplicates            1 -> 0\n"
        );
    }

    #[test]
    fn first_release_has_no_base_line() {
        let d = json!({"candidate": {"version": "draft", "runs": 0, "successes": 0,
            "failed": 0, "timedOut": 0, "passK": {"k": 2, "note": "needs 2"}}});
        let s = render_diff(&d);
        assert!(
            s.starts_with("draft: no earlier release to compare with\n"),
            "{s}"
        );
    }
}
