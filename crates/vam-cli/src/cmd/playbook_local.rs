//! Local `vam playbook` subcommands. They read or write a file and never contact `vamd`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use vam_spec::encoding;
use vam_spec::playbook::migrate_playbook_spec;
use vam_spec::v1::{PlaybookSpec, PlaybookStep, ResourceId, SideEffectClass, StepKind, StepMode};
use vam_spec::validation::{bound_step_args_digest, Validate};

use super::catalog::load_json_proto;
use crate::output::{emit, fail, ExitCode};

const SCAFFOLD_URN: &str = "urn:vam:playbook:local.vam.dev/acme/new-playbook";

/// `vam playbook init`: write one agentic task step, refusing to overwrite unless `force`.
pub fn init(out: &str, force: bool) -> Result<()> {
    let path = Path::new(out);
    if path.exists() && !force {
        return Err(fail(
            ExitCode::Conflict,
            format!("{out} already exists; pass --force to overwrite it"),
        ));
    }

    let spec = scaffold();
    spec.validate().map_err(|e| {
        fail(
            ExitCode::Invalid,
            format!("scaffold does not validate: {e}"),
        )
    })?;

    let json = encoding::proto_to_json("vam.v1.PlaybookSpec", &spec).map_err(anyhow::Error::msg)?;
    let pretty = serde_json::to_string_pretty(&serde_json::from_str::<Value>(&json)?)?;
    fs::write(path, format!("{pretty}\n")).with_context(|| format!("write {out}"))?;

    emit(json!({ "written": out }), || format!("Wrote {out}"));
    Ok(())
}

/// `vam playbook explain`: load, migrate and validate a file, then print its shape.
pub fn explain(path: &str) -> Result<()> {
    let mut spec: PlaybookSpec = load_json_proto(path, "vam.v1.PlaybookSpec")
        .map_err(|e| fail(ExitCode::Invalid, format!("{path}: {e:#}")))?;
    migrate_playbook_spec(&mut spec);
    spec.validate()
        .map_err(|e| fail(ExitCode::Invalid, format!("{path} does not validate: {e}")))?;

    let value = summarize(&spec);
    emit(value.clone(), || render(&value));
    Ok(())
}

/// Print a `vam playbook test` report. Exits `Failure` when any trial failed.
pub fn print_test_report(body: &str) -> Result<()> {
    let report: Value = serde_json::from_str(body).context("parse playbook test report")?;
    let passed = report["passed"].as_u64().unwrap_or(0);
    let repeat = report["repeat"].as_u64().unwrap_or(0);
    let pass_k = report["passK"].as_f64().unwrap_or(0.0);
    emit(report.clone(), || {
        let mut out = format!("passed {passed}/{repeat}  passK {pass_k:.2}");
        for failure in report["failures"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            out.push_str(&format!("\n  {failure}"));
        }
        out
    });
    if repeat > 0 && passed == repeat {
        Ok(())
    } else {
        Err(fail(
            ExitCode::Failure,
            format!("{passed}/{repeat} trials passed"),
        ))
    }
}

fn scaffold() -> PlaybookSpec {
    PlaybookSpec {
        activation: Some(vam_spec::v1::PlaybookActivation::asking(
            "Use when the user wants this procedure run. Replace this with one or two sentences.",
        )),
        resource_id: Some(ResourceId {
            urn: SCAFFOLD_URN.to_string(),
        }),
        display_name: "New playbook".to_string(),
        description: "One agentic task. Replace the instruction with the procedure.".to_string(),
        steps: vec![PlaybookStep {
            id: "task".to_string(),
            description: "Do the task".to_string(),
            instruction: "Describe what the agent should do, step by step.".to_string(),
            kind: StepKind::Task as i32,
            mode: StepMode::Agentic as i32,
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Shape of a playbook as the `explain` JSON: steps, gates, guards, and mode counts.
///
/// `ratio` is `(bound + assisted) / task steps`. Gates are not task steps, and a playbook with
/// no task steps reports `0`.
pub fn summarize(spec: &PlaybookSpec) -> Value {
    let mut steps = Vec::new();
    let mut gates = Vec::new();
    let (mut bound, mut assisted, mut agentic, mut tasks) = (0u32, 0u32, 0u32, 0u32);

    for step in &spec.steps {
        let kind = StepKind::try_from(step.kind).unwrap_or(StepKind::Unspecified);
        let mode = StepMode::try_from(step.mode).unwrap_or(StepMode::Unspecified);

        if kind == StepKind::Gate {
            gates.push(json!({
                "id": step.id,
                "question": step.question,
                "options": step.options.iter().map(|o| o.label.clone()).collect::<Vec<_>>(),
                "responseSchema": struct_value(step.response_schema.as_ref()),
                "choicesFrom": step.choices_from,
                "timeoutSeconds": step.timeout.as_ref().map(|d| d.seconds),
            }));
        } else {
            tasks += 1;
            match mode {
                StepMode::Bound => bound += 1,
                StepMode::Assisted => assisted += 1,
                StepMode::Agentic => agentic += 1,
                StepMode::Unspecified => {}
            }
        }

        steps.push(json!({
            "id": step.id,
            "kind": kind.as_str_name(),
            "mode": (kind != StepKind::Gate).then(|| mode.as_str_name()),
            "outcomes": step.outcomes.iter().map(|o| o.label.clone()).collect::<Vec<_>>(),
            "after": step.after,
            "afterAny": step.after_any,
            "argsDigest": bound_step_args_digest(step),
        }));
    }

    let guards: Vec<Value> = spec
        .guards
        .iter()
        .map(|guard| {
            json!({
                "tools": guard.tools,
                "sideEffects": guard
                    .side_effects
                    .iter()
                    .map(|s| SideEffectClass::try_from(*s).unwrap_or(SideEffectClass::Unspecified).as_str_name())
                    .collect::<Vec<_>>(),
                "allowedWhen": guard.allowed_when,
                "argsWithin": guard
                    .args_within
                    .iter()
                    .map(|a| json!({ "argument": a.argument, "within": a.within }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    let ratio = if tasks == 0 {
        0.0
    } else {
        f64::from(bound + assisted) / f64::from(tasks)
    };

    json!({
        "steps": steps,
        "gates": gates,
        "guards": guards,
        "modes": {
            "bound": bound,
            "assisted": assisted,
            "agentic": agentic,
            "ratio": ratio,
        },
        "warnings": vam_spec::validation::playbook_enforcement_warnings(spec),
    })
}

fn struct_value(s: Option<&prost_types::Struct>) -> Value {
    s.and_then(|s| encoding::proto_to_json("google.protobuf.Struct", s).ok())
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or(Value::Null)
}

fn render(value: &Value) -> String {
    let mut out = String::from("Steps\n");
    for step in value["steps"].as_array().into_iter().flatten() {
        let mode = step["mode"].as_str().unwrap_or("-");
        out.push_str(&format!(
            "  {}  {}  {}  outcomes: [{}]  after: [{}]  afterAny: [{}]",
            str_of(&step["id"]),
            str_of(&step["kind"]),
            mode,
            join(&step["outcomes"]),
            join(&step["after"]),
            join(&step["afterAny"]),
        ));
        if mode == "STEP_MODE_BOUND" {
            let digest = if step["argsDigest"].is_null() {
                "absent"
            } else {
                "present"
            };
            out.push_str(&format!("  args digest: {digest}"));
        }
        out.push('\n');
    }

    out.push_str("Gates\n");
    let gates = value["gates"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if gates.is_empty() {
        out.push_str("  none\n");
    }
    for gate in gates {
        out.push_str(&format!(
            "  {}: {}\n",
            str_of(&gate["id"]),
            str_of(&gate["question"])
        ));
        out.push_str(&format!("    options: [{}]\n", join(&gate["options"])));
        if !gate["responseSchema"].is_null() {
            out.push_str(&format!("    responseSchema: {}\n", gate["responseSchema"]));
        }
        if let Some(from) = gate["choicesFrom"].as_str().filter(|s| !s.is_empty()) {
            out.push_str(&format!("    choicesFrom: {from}\n"));
        }
        if let Some(secs) = gate["timeoutSeconds"].as_i64() {
            out.push_str(&format!("    timeout: {secs}s\n"));
        }
    }

    out.push_str("Guards\n");
    let guards = value["guards"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if guards.is_empty() {
        out.push_str("  none\n");
    }
    for guard in guards {
        out.push_str(&format!(
            "  tools: [{}]  sideEffects: [{}]  allowedWhen: [{}]\n",
            join(&guard["tools"]),
            join(&guard["sideEffects"]),
            join(&guard["allowedWhen"]),
        ));
        for within in guard["argsWithin"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "    argsWithin: {} within {}\n",
                str_of(&within["argument"]),
                str_of(&within["within"]),
            ));
        }
    }

    let modes = &value["modes"];
    out.push_str(&format!(
        "Modes\n  bound {}  assisted {}  agentic {}  ratio {:.2}",
        modes["bound"],
        modes["assisted"],
        modes["agentic"],
        modes["ratio"].as_f64().unwrap_or(0.0),
    ));

    for warning in value["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        out.push_str(&format!("\nWarning: {warning}"));
    }
    out
}

fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn join(v: &Value) -> String {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::classify;

    const JIRA: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/jira-assistant/playbook.json"
    );

    fn spec_from_json(json: &str) -> PlaybookSpec {
        encoding::json_to_proto("vam.v1.PlaybookSpec", json).expect("spec json decodes")
    }

    #[test]
    fn init_writes_a_valid_file_and_refuses_to_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("playbook.json");
        let out = path.to_str().expect("utf8 path");

        init(out, false).expect("first init writes the file");
        let loaded: PlaybookSpec =
            load_json_proto(out, "vam.v1.PlaybookSpec").expect("written file decodes");
        loaded.validate().expect("written file validates");

        let before = fs::read_to_string(&path).expect("read back");
        let err = init(out, false).expect_err("second init without --force must fail");
        assert_eq!(classify(&err).0, ExitCode::Conflict);
        assert_eq!(fs::read_to_string(&path).expect("read back"), before);

        init(out, true).expect("--force overwrites");
    }

    #[test]
    fn explain_summarises_the_jira_example() {
        let mut spec: PlaybookSpec =
            load_json_proto(JIRA, "vam.v1.PlaybookSpec").expect("jira example decodes");
        migrate_playbook_spec(&mut spec);
        spec.validate().expect("jira example validates");

        let value = summarize(&spec);
        let gate = &value["gates"][0];
        assert_eq!(gate["id"], "request-approver-review");
        assert_eq!(gate["options"], json!(["approved", "rejected"]));
        assert_eq!(
            gate["choicesFrom"],
            "steps.query-stale-issues.output.issues"
        );

        let guard = &value["guards"][0];
        assert_eq!(guard["tools"], json!(["jira_transition_issue"]));
        assert_eq!(guard["argsWithin"][0]["argument"], "issue_key");

        assert_eq!(value["modes"]["agentic"], 3);
        assert_eq!(value["modes"]["ratio"], 0.0);
        assert_eq!(value["warnings"], json!([]));
    }

    #[test]
    fn ratio_counts_bound_and_assisted_over_task_steps() {
        let spec = spec_from_json(
            r#"{
                "resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/mixed"},
                "displayName": "Mixed",
                "steps": [
                    {"id": "lookup", "kind": "STEP_KIND_TASK", "mode": "STEP_MODE_BOUND",
                     "instruction": "Look up.",
                     "binding": {"toolName": "lookup", "arguments": {"q": "x"}}},
                    {"id": "summarise", "kind": "STEP_KIND_TASK", "mode": "STEP_MODE_AGENTIC",
                     "instruction": "Summarise.", "after": ["lookup"]}
                ]
            }"#,
        );

        let value = summarize(&spec);
        assert_eq!(value["modes"]["bound"], 1);
        assert_eq!(value["modes"]["agentic"], 1);
        assert!((value["modes"]["ratio"].as_f64().expect("ratio") - 0.5).abs() < f64::EPSILON);
        assert!(value["steps"][0]["argsDigest"].is_string());
        assert!(value["steps"][1]["argsDigest"].is_null());
    }

    #[test]
    fn a_fresh_scaffold_is_tracking_only_and_explain_says_so() {
        let value = summarize(&scaffold());
        assert_eq!(
            value["warnings"],
            json!([vam_spec::validation::STEP_TRACKING_ONLY_WARNING])
        );
        assert!(render(&value).contains("Warning: This playbook enforces only step tracking"));
    }

    #[test]
    fn ratio_is_zero_without_task_steps() {
        let spec = spec_from_json(
            r#"{"resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/empty"}}"#,
        );
        assert_eq!(summarize(&spec)["modes"]["ratio"], 0.0);
    }
}
