//! Console lines, `summary.json` and `brief.md`.

use crate::driver::{class_action, class_name, ScenarioResult};
use crate::runner::{Outcome, Report};
use crate::scenario::Scenario;
use serde_json::{json, Value};
use std::path::Path;

pub fn report_name(r: Report) -> &'static str {
    match r {
        Report::Pass => "pass",
        Report::Fail => "fail",
        Report::RedRightReason => "red_right_reason",
        Report::RedWrongReason => "red_wrong_reason",
    }
}

fn label(r: Report) -> &'static str {
    match r {
        Report::Pass => "PASS",
        Report::Fail => "FAIL",
        Report::RedRightReason => "RED (right reason)",
        Report::RedWrongReason => "RED (wrong reason)",
    }
}

pub fn line(s: &Scenario, r: &ScenarioResult) -> String {
    let at = match &r.outcome {
        Outcome::Passed => String::new(),
        Outcome::Failed { position, class } => {
            format!("  at {position}, class {}", class_name(*class))
        }
    };
    format!(
        "{:<20} {}  ({} ms){at}",
        label(r.report),
        s.id,
        r.duration_ms
    )
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(n).collect::<String>())
    }
}

/// Files that match a guard glob like `vam:crates/x/src/**`: the part before the first `*`.
pub fn expand_guard(repo: &Path, guard: &str) -> Vec<String> {
    let g = guard.strip_prefix("vam:").unwrap_or(guard);
    let prefix = g.split('*').next().unwrap_or(g);
    let base = repo.join(prefix.trim_end_matches('/'));
    let mut out = Vec::new();
    fn walk(dir: &Path, repo: &Path, out: &mut Vec<String>) {
        if out.len() >= 15 {
            return;
        }
        if dir.is_file() {
            if let Ok(p) = dir.strip_prefix(repo) {
                out.push(p.display().to_string());
            }
            return;
        }
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut v: Vec<_> = rd.flatten().map(|e| e.path()).collect();
            v.sort();
            for p in v {
                walk(&p, repo, out);
            }
        }
    }
    walk(&base, repo, &mut out);
    if out.is_empty() {
        out.push(format!("{g} (nothing found)"));
    }
    out
}

pub fn brief(s: &Scenario, r: &ScenarioResult, repo: &Path) -> String {
    let mut m = String::new();
    m.push_str(&format!("# {} - {}\n\n", s.id, s.title));
    m.push_str(&format!(
        "- status: `{:?}`, needs: `{:?}`\n- report: **{}**\n",
        s.status,
        s.needs,
        label(r.report)
    ));
    if let Outcome::Failed { position, class } = &r.outcome {
        m.push_str(&format!(
            "- failed step: `{position}`\n- class: **{}** ({})\n",
            class_name(*class),
            class_action(*class)
        ));
        if let Some((_, step)) = s.steps().find(|(p, _)| p == position) {
            m.push_str(&format!("\n## Step\n\n```\n{step:?}\n```\n"));
        }
        m.push_str(&format!(
            "\n## Expected vs observed\n\nExpected: {}\n\nObserved: {}\n",
            r.expected, r.observed
        ));
    }
    if let Some(ef) = &s.expect_fail {
        m.push_str(&format!(
            "\n## Expected failure\n\n`expectFail {{step: {}, class: {:?}}}`\n",
            ef.step, ef.class
        ));
    }
    if let Some(ref rf) = r.evidence.refusal {
        m.push_str(&format!("\n## Last refused start\n\n{}\n", clip(rf, 400)));
    }
    for run in &r.evidence.runs {
        m.push_str(&format!(
            "\n## Run `{}` (agent `{}`)\n\nstate: `{}`\n\n",
            run.run_id, run.agent, run.state
        ));
        m.push_str("### Last events\n\n```\n");
        let n = run.events.len();
        for e in &run.events[n.saturating_sub(20)..] {
            m.push_str(&format!(
                "{} {} {}\n",
                e["sequence"],
                e["eventType"].as_str().unwrap_or(""),
                clip(&e["payload"].to_string(), 200)
            ));
        }
        m.push_str("```\n\n### Mock trace\n\n```\n");
        for e in run.events.iter().filter(|e| {
            e["eventType"]
                .as_str()
                .unwrap_or("")
                .starts_with("vam.v1.mock.")
        }) {
            m.push_str(&format!(
                "{} {}\n",
                e["eventType"].as_str().unwrap_or(""),
                clip(&e["payload"].to_string(), 200)
            ));
        }
        m.push_str("```\n");
    }
    m.push_str(&format!(
        "\n## vamd log (tail)\n\n```\n{}\n```\n",
        r.evidence.vamd_log
    ));
    for (p, tail) in &r.evidence.guest_logs {
        m.push_str(&format!(
            "\n## Guest log `{}` (tail)\n\n```\n{tail}\n```\n",
            p.display()
        ));
    }
    m.push_str("\n## Suspects (from `guards`)\n\n");
    for g in &s.guards {
        for f in expand_guard(repo, g) {
            m.push_str(&format!("- {f}\n"));
        }
    }
    m
}

/// The runs of a trial: agent, workspace, thread and run id of each.
pub fn trial_identity(r: &ScenarioResult) -> Value {
    json!({
        "timedOut": r.timed_out,
        "runs": r.evidence.runs.iter().map(|x| json!({
            "agent": x.agent_urn,
            "workspace": x.workspace_id,
            "thread": x.thread_ref,
            "runId": x.run_id,
        })).collect::<Vec<_>>(),
    })
}

/// `trials/<scenario>--<model>--<n>`: the folder and brief stem of one bench trial.
pub fn trial_stem(id: &str, model: &str, n: u32) -> String {
    let m: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{id}--{m}--{n}")
}

/// Writes one bench trial's folder: `identity.json` and the thread of each run
/// (`thread.json`, then `thread-2.json`, ...). Host-recorded messages only; no key is held here.
pub fn write_trial(
    out: &Path,
    stem: &str,
    model: &str,
    n: u32,
    r: &ScenarioResult,
) -> std::io::Result<()> {
    let dir = out.join("trials").join(stem);
    std::fs::create_dir_all(&dir)?;
    let mut id = trial_identity(r);
    id["model"] = json!(model);
    id["trial"] = json!(n);
    std::fs::write(
        dir.join("identity.json"),
        serde_json::to_vec_pretty(&id).unwrap_or_default(),
    )?;
    let mut seen: Vec<String> = Vec::new();
    for (run, extra) in r.evidence.runs.iter().zip(&r.evidence.extras) {
        let (Some(t), key) = (
            &extra.thread,
            format!("{}/{}", run.workspace_id, run.thread_ref),
        ) else {
            continue;
        };
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let name = if seen.len() == 1 {
            "thread.json".to_string()
        } else {
            format!("thread-{}.json", seen.len())
        };
        std::fs::write(
            dir.join(name),
            serde_json::to_vec_pretty(t).unwrap_or_default(),
        )?;
    }
    Ok(())
}

// Each argument is a distinct report field; a params struct would be a refactor.
#[allow(clippy::too_many_arguments)]
pub fn summary(
    tag: &str,
    started: &str,
    duration_ms: u128,
    vamd: Value,
    rows: &[(&Scenario, &ScenarioResult, Option<String>)],
    skipped: &[(String, String)],
    exit: i32,
    mock: bool,
    trials: &[Option<(String, u32)>],
) -> Value {
    let catalog = vam_inference::ModelCatalog::new();
    let count = |r: Report| rows.iter().filter(|(_, x, _)| x.report == r).count();
    json!({
        "runTag": tag,
        "startedAt": started,
        "durationMs": duration_ms,
        "vamd": vamd,
        "totals": {
            "pass": count(Report::Pass),
            "fail": count(Report::Fail),
            "redRightReason": count(Report::RedRightReason),
            "redWrongReason": count(Report::RedWrongReason),
            "skipped": skipped.len(),
        },
        "exit": exit,
        "mock": mock,
        "bench": crate::engine::channels::merge_summary(crate::engine::notifications::Probes::summary()),
        "scenarios": rows.iter().enumerate().map(|(i, (s, r, brief))| {
            let (step, class) = match &r.outcome {
                Outcome::Failed { position, class } => (json!(position), json!(class_name(*class))),
                Outcome::Passed => (Value::Null, Value::Null),
            };
            let mut v = json!({
                "id": s.id,
                "status": format!("{:?}", s.status).to_lowercase(),
                "report": report_name(r.report),
                "durationMs": r.duration_ms,
                "failedStep": step,
                "class": class,
                "expected": r.expected,
                "observed": r.observed,
                "brief": brief,
            });
            // A bench trial names its model and number and the identity it ran on, so isolation
            // can be checked after the run. Absent outside a bench.
            if let (Some(o), Some(Some((model, n)))) = (v.as_object_mut(), trials.get(i)) {
                o.insert("model".into(), json!(model));
                o.insert("trial".into(), json!(n));
                o.insert("identity".into(), trial_identity(r));
            }
            // Usage, harness, repairs and loop detections (H-0.8), on every run. The node is the
            // mock, so the numbers are estimates and never scored (benchmark decision 4).
            if let (Some(o), Some(extra)) = (v.as_object_mut(), r.measure(&catalog).summary_fields().as_object()) {
                o.extend(extra.clone());
            }
            v
        }).collect::<Vec<_>>(),
        "skipped": skipped.iter().map(|(id, why)| json!({"id": id, "reason": why})).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod trial_tests {
    use super::*;
    use crate::driver::Evidence;
    use crate::engine::RunRec;
    use crate::runner::{Outcome, Report};
    use crate::usage::RunExtra;

    fn scenario() -> Scenario {
        serde_yaml::from_str(
            "id: some-task\ntitle: t\ntags: [chat]\nneeds: process\ngiven: [org-admin]\nwhen:\n  - chat: hi\nthen:\n  - expect: { run: succeeded }\nstatus: active\n",
        )
        .expect("scenario parses")
    }

    fn result(n: u32) -> ScenarioResult {
        let mut run = RunRec::default();
        run.agent_urn = format!("urn:vam:agent:local/agent-k{n}");
        run.workspace_id = format!("ws-{n}");
        run.thread_ref = format!("t1-k{n}");
        run.run_id = format!("run-{n}");
        ScenarioResult {
            outcome: Outcome::Passed,
            report: Report::Pass,
            expected: String::new(),
            observed: String::new(),
            duration_ms: 1,
            timed_out: false,
            evidence: Evidence {
                runs: vec![run],
                extras: vec![RunExtra {
                    receipt: None,
                    thread: Some(
                        json!({ "records": [{ "role": "user", "content": format!("hello {n}") }] }),
                    ),
                }],
                refusal: None,
                vamd_log: String::new(),
                guest_logs: vec![],
                ledger: vec![],
            },
        }
    }

    fn summarize(mock: bool, trials: &[Option<(String, u32)>]) -> Value {
        let s = scenario();
        let rs: Vec<ScenarioResult> = (1..=trials.len() as u32).map(result).collect();
        let rows: Vec<(&Scenario, &ScenarioResult, Option<String>)> =
            rs.iter().map(|r| (&s, r, None)).collect();
        summary("tag", "now", 1, json!({}), &rows, &[], 0, mock, trials)
    }

    #[test]
    fn summary_mock_flag_follows_the_run_and_identity_is_for_bench_only() {
        let bench: Vec<Option<(String, u32)>> = (1..=3)
            .map(|n| Some(("mock-model".to_string(), n)))
            .collect();
        let v = summarize(true, &bench);
        assert_eq!(v["mock"], json!(true));
        let ws: Vec<&str> = v["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["identity"]["runs"][0]["workspace"].as_str().unwrap())
            .collect();
        assert_eq!(ws, ["ws-1", "ws-2", "ws-3"]);
        assert_eq!(v["scenarios"][1]["trial"], json!(2));
        assert_eq!(summarize(false, &bench)["mock"], json!(false));
        let plain = summarize(true, &[None]);
        assert!(plain["scenarios"][0].get("identity").is_none());
        assert!(plain["scenarios"][0].get("trial").is_none());
    }

    #[test]
    fn each_trial_gets_its_own_brief_name_and_folder() {
        let stems: Vec<String> = [("a/b", 1), ("a/b", 2), ("c", 1)]
            .iter()
            .map(|(m, n)| trial_stem("some-task", m, *n))
            .collect();
        assert_eq!(stems[0], "some-task--a_b--1");
        assert_eq!(
            stems.iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );
    }

    #[test]
    fn trial_folders_hold_identity_and_thread_and_no_secret() {
        let out = std::env::temp_dir().join(format!("vam-e2e-trials-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        // A key in the environment is never read into a trial folder.
        std::env::set_var("VAM_E2E_TRIAL_CANARY", "sk-canary-must-not-appear");
        for n in 1..=3 {
            let stem = trial_stem("some-task", "mock-model", n);
            write_trial(&out, &stem, "mock-model", n, &result(n)).unwrap();
        }
        let mut all = String::new();
        for n in 1..=3 {
            let d = out
                .join("trials")
                .join(format!("some-task--mock-model--{n}"));
            let id: Value =
                serde_json::from_slice(&std::fs::read(d.join("identity.json")).unwrap()).unwrap();
            assert_eq!(id["runs"][0]["thread"], json!(format!("t1-k{n}")));
            for f in ["identity.json", "thread.json"] {
                all.push_str(&std::fs::read_to_string(d.join(f)).unwrap());
            }
        }
        assert!(all.contains("hello 2"));
        assert!(
            !all.contains("sk-") && !all.contains("canary"),
            "a secret-shaped string was written"
        );
        let _ = std::fs::remove_dir_all(&out);
    }
}
