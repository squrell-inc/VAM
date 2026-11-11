//! Scripted playbook scenarios, run against a fresh in-memory workflow store for each trial.
//!
//! Tool actions are mocked observations: nothing is dispatched to a guest, MCP server or HSP.
//! Guards, evidence, gates and the finish check are the host's own functions, so a scenario
//! tests the rules a real run would enforce rather than a second copy of them.

use serde::{Deserialize, Serialize};
use vam_spec::v1::{PlaybookSpec, ResourceId, SideEffectClass};
use vam_spec::validation::Validate;
use vam_telemetry::TelemetryCapture;

use crate::error::OrchestratorError;
use crate::events::args_digest_from_json;
use crate::security::{playbook_guard_denial, PlaybookPepView};
use crate::trajectory::{emit_guest_tool_outcome, emit_tool_invoked};
use crate::users::LOCAL_USER_ID;
use crate::workflow::{
    PlaybookFinishCheck, PlaybookRunPin, PlaybookStepCompletion, SqliteWorkflowStore,
};

/// Largest `--repeat` the runner accepts.
pub const MAX_REPEAT: u32 = 30;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scenario {
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(default)]
    pub expect: Expect,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    Tool(ToolAction),
    StepComplete(StepCompleteAction),
    Gate(GateAction),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAction {
    pub name: String,
    /// `SIDE_EFFECT_CLASS_*` name. Empty means unspecified.
    #[serde(default)]
    pub side_effect: String,
    /// `success` or `failure`.
    #[serde(default = "default_result")]
    pub result: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

fn default_result() -> String {
    "success".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepCompleteAction {
    pub step_id: String,
    pub outcome: String,
    #[serde(default)]
    pub output: Option<serde_json::Value>,
    #[serde(default)]
    pub agent_summary: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateAction {
    pub step_id: String,
    /// `SUBMITTED` or `REJECTED`.
    pub decision: String,
    #[serde(default)]
    pub response: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Expect {
    #[serde(default)]
    pub steps: Vec<ExpectStep>,
    /// Tools a guard denied, in any order.
    #[serde(default)]
    pub denied_tools: Vec<String>,
    /// `complete` or `incomplete`, checked after the actions.
    #[serde(default)]
    pub finish: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectStep {
    pub id: String,
    /// Host step state name, for example `completed` or `open`.
    pub state: String,
    #[serde(default)]
    pub outcome: Option<String>,
}

/// One trial's verdict. `failures` names every assertion that did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trial {
    pub passed: bool,
    pub failures: Vec<String>,
}

/// The `vam playbook test` report.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestReport {
    pub passed: u32,
    pub repeat: u32,
    /// `(passed / repeat)^repeat`. All trials passing gives `1`.
    pub pass_k: f64,
    pub failures: Vec<String>,
}

/// Run `scenario` against `spec` `repeat` times, each on a fresh store.
pub fn run_playbook_test(
    spec: &PlaybookSpec,
    scenario: &Scenario,
    repeat: u32,
) -> Result<TestReport, OrchestratorError> {
    if !(1..=MAX_REPEAT).contains(&repeat) {
        return Err(OrchestratorError::InvalidContract(format!(
            "repeat must be between 1 and {MAX_REPEAT}"
        )));
    }
    spec.validate()
        .map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?;
    let mut spec = spec.clone();
    vam_spec::playbook::migrate_playbook_spec(&mut spec);

    let mut trials = Vec::new();
    for index in 0..repeat {
        trials.push(run_trial(&spec, scenario, index)?);
    }
    Ok(aggregate(&trials))
}

/// Fold trial verdicts into the report. `passK` is `(passed / repeat)^repeat`.
pub fn aggregate(trials: &[Trial]) -> TestReport {
    let repeat = trials.len() as u32;
    let passed = trials.iter().filter(|t| t.passed).count() as u32;
    let pass_k = if repeat == 0 {
        0.0
    } else {
        (f64::from(passed) / f64::from(repeat)).powi(repeat as i32)
    };
    let failures = trials
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.passed)
        .flat_map(|(i, t)| {
            t.failures
                .iter()
                .map(move |f| format!("trial {}: {f}", i + 1))
        })
        .collect();
    TestReport {
        passed,
        repeat,
        pass_k,
        failures,
    }
}

fn run_trial(
    spec: &PlaybookSpec,
    scenario: &Scenario,
    index: u32,
) -> Result<Trial, OrchestratorError> {
    let store = SqliteWorkflowStore::in_memory()?;
    let run_id = format!("urn:vam:run:local.vam.dev/playbook-test/trial-{index}");
    store.create_run(&ResourceId {
        urn: run_id.clone(),
    })?;

    let spec_json = vam_spec::encoding::proto_to_json("vam.v1.PlaybookSpec", spec)
        .map_err(OrchestratorError::InvalidContract)?;
    let pin = PlaybookRunPin {
        urn: spec
            .resource_id
            .as_ref()
            .map(|r| r.urn.clone())
            .unwrap_or_default(),
        version: "test".to_string(),
        digest: args_digest_from_json(
            &serde_json::from_str(&spec_json)
                .map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?,
        ),
    };
    store.init_playbook_run(&run_id, &pin, spec)?;

    let mut failures = Vec::new();
    let mut denied = Vec::new();
    for action in &scenario.actions {
        match action {
            Action::Tool(tool) => {
                if run_tool(&store, &run_id, spec, tool)? {
                    denied.push(tool.name.clone());
                }
            }
            Action::StepComplete(step) => {
                let completion = PlaybookStepCompletion {
                    outcome: step.outcome.clone(),
                    output_json: step.output.as_ref().map(|v| v.to_string()),
                    agent_summary: step.agent_summary.clone(),
                };
                if let Err(err) = store.complete_playbook_step(&run_id, &step.step_id, &completion)
                {
                    failures.push(format!(
                        "stepComplete '{}' was rejected: {err}",
                        step.step_id
                    ));
                }
            }
            Action::Gate(gate) => {
                let request = ResourceId {
                    urn: format!("{run_id}/gate/{}", gate.step_id),
                };
                let token = ResourceId {
                    urn: format!(
                        "urn:vam:token:local.vam.dev/playbook-test/{index}-{}",
                        gate.step_id
                    ),
                };
                let response = gate.response.as_ref().map(|v| v.to_string());
                if let Err(err) = store.decide_approval(
                    &request,
                    &token,
                    &gate.decision,
                    "playbook-test",
                    LOCAL_USER_ID,
                    None,
                    None,
                    None,
                    response.as_deref(),
                ) {
                    failures.push(format!("gate '{}' was rejected: {err}", gate.step_id));
                }
            }
        }
    }

    let steps = store.list_playbook_steps(&run_id)?;
    for expected in &scenario.expect.steps {
        match steps.iter().find(|s| s.step_id == expected.id) {
            None => failures.push(format!("step '{}' is not in the run", expected.id)),
            Some(actual) => {
                if actual.state.as_str() != expected.state {
                    failures.push(format!(
                        "step '{}' is {} but expected {}",
                        expected.id,
                        actual.state.as_str(),
                        expected.state
                    ));
                }
                if let Some(outcome) = &expected.outcome {
                    if &actual.outcome != outcome {
                        failures.push(format!(
                            "step '{}' outcome is '{}' but expected '{outcome}'",
                            expected.id, actual.outcome
                        ));
                    }
                }
            }
        }
    }

    let mut denied_sorted = denied.clone();
    denied_sorted.sort();
    let mut expected_denied = scenario.expect.denied_tools.clone();
    expected_denied.sort();
    if denied_sorted != expected_denied {
        failures.push(format!(
            "denied tools were {denied:?} but expected {:?}",
            scenario.expect.denied_tools
        ));
    }

    if let Some(expected) = &scenario.expect.finish {
        let actual = match store.check_playbook_finish(&run_id, None)? {
            Some(PlaybookFinishCheck::Complete) => "complete",
            Some(PlaybookFinishCheck::Incomplete { .. }) => "incomplete",
            None => "none",
        };
        if actual != expected {
            failures.push(format!("finish is {actual} but expected {expected}"));
        }
    }

    Ok(Trial {
        passed: failures.is_empty(),
        failures,
    })
}

/// Apply one tool observation. Returns `true` when a guard denied it.
///
/// A denied call writes no event. An allowed call writes the invoked event and the outcome
/// event, which is what the evidence check reads.
fn run_tool(
    store: &SqliteWorkflowStore,
    run_id: &str,
    spec: &PlaybookSpec,
    tool: &ToolAction,
) -> Result<bool, OrchestratorError> {
    let side_effect = parse_side_effect(&tool.side_effect)?;
    let view = PlaybookPepView {
        steps: store.list_playbook_steps(run_id)?,
        guards: spec.guards.clone(),
    };
    if playbook_guard_denial(&view, &tool.name, side_effect, &tool.arguments).is_some() {
        return Ok(true);
    }

    let digest = args_digest_from_json(&tool.arguments);
    emit_tool_invoked(
        store,
        run_id,
        &tool.name,
        &digest,
        None,
        TelemetryCapture::None,
        None,
        None,
    );
    let failed = tool.result != "success";
    emit_guest_tool_outcome(
        store,
        run_id,
        &serde_json::json!({
            "tool_name": tool.name,
            "exit_code": if failed { 1 } else { 0 },
        }),
        failed,
    );
    Ok(false)
}

fn parse_side_effect(name: &str) -> Result<i32, OrchestratorError> {
    if name.is_empty() {
        return Ok(SideEffectClass::Unspecified as i32);
    }
    SideEffectClass::from_str_name(name)
        .map(|class| class as i32)
        .ok_or_else(|| {
            OrchestratorError::InvalidContract(format!("unknown side effect class '{name}'"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: &str) -> PlaybookSpec {
        vam_spec::encoding::json_to_proto("vam.v1.PlaybookSpec", json).expect("spec decodes")
    }

    fn scenario(json: &str) -> Scenario {
        serde_json::from_str(json).expect("scenario decodes")
    }

    /// One agentic task whose completion needs a successful `lookup` call first.
    fn evidence_spec() -> PlaybookSpec {
        spec(
            r#"{
                "resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/evidence"},
                "displayName": "Evidence",
                "steps": [{
                    "id": "lookup-step", "kind": "STEP_KIND_TASK", "mode": "STEP_MODE_AGENTIC",
                    "instruction": "Look it up.",
                    "evidence": {"tool": "lookup", "result": "success"}
                }]
            }"#,
        )
    }

    /// A gate that must be approved before a guarded tool may run.
    fn guarded_spec() -> PlaybookSpec {
        spec(
            r#"{
                "resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/guarded"},
                "displayName": "Guarded",
                "steps": [
                    {"id": "approve", "kind": "STEP_KIND_GATE", "question": "Proceed?",
                     "options": [{"label": "approved"}, {"label": "rejected"}]},
                    {"id": "transition", "kind": "STEP_KIND_TASK", "mode": "STEP_MODE_AGENTIC",
                     "instruction": "Transition.", "after": ["approve:approved"]}
                ],
                "guards": [{"tools": ["jira_transition_issue"],
                            "allowedWhen": ["approve:approved"]}]
            }"#,
        )
    }

    fn passing_evidence_scenario() -> Scenario {
        scenario(
            r#"{
                "actions": [
                    {"tool": {"name": "lookup", "sideEffect": "SIDE_EFFECT_CLASS_READ_ONLY",
                              "result": "success", "arguments": {}}},
                    {"stepComplete": {"stepId": "lookup-step", "outcome": "done"}}
                ],
                "expect": {"steps": [{"id": "lookup-step", "state": "completed", "outcome": "done"}],
                           "finish": "complete"}
            }"#,
        )
    }

    #[test]
    fn completed_steps_with_evidence_pass_every_trial() {
        let report =
            run_playbook_test(&evidence_spec(), &passing_evidence_scenario(), 3).expect("runs");
        assert_eq!(report.passed, 3);
        assert_eq!(report.repeat, 3);
        assert_eq!(report.pass_k, 1.0);
        assert!(report.failures.is_empty());
    }

    #[test]
    fn a_guarded_tool_before_its_gate_is_denied_and_listed() {
        let scenario = scenario(
            r#"{
                "actions": [
                    {"tool": {"name": "jira_transition_issue", "sideEffect": "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL"}},
                    {"gate": {"stepId": "approve", "decision": "SUBMITTED", "response": {"option": "approved"}}},
                    {"tool": {"name": "jira_transition_issue", "sideEffect": "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL"}},
                    {"stepComplete": {"stepId": "transition", "outcome": "done"}}
                ],
                "expect": {
                    "steps": [{"id": "approve", "state": "completed", "outcome": "approved"},
                              {"id": "transition", "state": "completed", "outcome": "done"}],
                    "deniedTools": ["jira_transition_issue"]
                }
            }"#,
        );
        let report = run_playbook_test(&guarded_spec(), &scenario, 1).expect("runs");
        assert_eq!(report.passed, 1, "failures: {:?}", report.failures);
    }

    #[test]
    fn a_denial_the_scenario_did_not_expect_fails_the_trial() {
        let scenario = scenario(
            r#"{
                "actions": [
                    {"tool": {"name": "jira_transition_issue", "sideEffect": "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL"}}
                ],
                "expect": {"steps": [], "deniedTools": []}
            }"#,
        );
        let report = run_playbook_test(&guarded_spec(), &scenario, 1).expect("runs");
        assert_eq!(report.passed, 0);
        assert!(report.failures[0].contains("denied tools"));
    }

    #[test]
    fn step_complete_without_the_evidence_fails_the_action() {
        let scenario = scenario(
            r#"{
                "actions": [{"stepComplete": {"stepId": "lookup-step", "outcome": "done"}}],
                "expect": {"steps": [{"id": "lookup-step", "state": "completed"}]}
            }"#,
        );
        let report = run_playbook_test(&evidence_spec(), &scenario, 1).expect("runs");
        assert_eq!(report.passed, 0);
        assert!(report
            .failures
            .iter()
            .any(|f| f.contains("stepComplete 'lookup-step' was rejected")));
    }

    #[test]
    fn an_open_gate_is_incomplete_until_it_is_decided() {
        let open = scenario(r#"{"expect": {"finish": "incomplete"}}"#);
        assert_eq!(
            run_playbook_test(&guarded_spec(), &open, 1)
                .expect("runs")
                .passed,
            1
        );

        // Approving the gate closes it. The run is only complete once the transition runs too.
        let decided = scenario(
            r#"{
                "actions": [
                    {"gate": {"stepId": "approve", "decision": "SUBMITTED",
                              "response": {"option": "approved"}}},
                    {"stepComplete": {"stepId": "transition", "outcome": "done"}}
                ],
                "expect": {
                    "steps": [{"id": "approve", "state": "completed", "outcome": "approved"}],
                    "finish": "complete"
                }
            }"#,
        );
        let report = run_playbook_test(&guarded_spec(), &decided, 1).expect("runs");
        assert_eq!(report.passed, 1, "failures: {:?}", report.failures);

        let approved_only = scenario(
            r#"{
                "actions": [{"gate": {"stepId": "approve", "decision": "SUBMITTED",
                                      "response": {"option": "approved"}}}],
                "expect": {"finish": "incomplete"}
            }"#,
        );
        let report = run_playbook_test(&guarded_spec(), &approved_only, 1).expect("runs");
        assert_eq!(report.passed, 1, "failures: {:?}", report.failures);
    }

    #[test]
    fn pass_k_is_passed_over_repeat_raised_to_repeat() {
        let pass = Trial {
            passed: true,
            failures: vec![],
        };
        let fail = Trial {
            passed: false,
            failures: vec!["boom".to_string()],
        };
        let report = aggregate(&[pass, fail]);
        assert_eq!(report.passed, 1);
        assert_eq!(report.repeat, 2);
        assert_eq!(report.pass_k, 0.25);
        assert_eq!(report.failures, vec!["trial 2: boom".to_string()]);
    }

    /// The shipped pr-review example and its scenario must keep passing, so the tutorial's
    /// `vam playbook test` output stays true.
    #[test]
    fn the_pr_review_example_scenario_passes() {
        let playbook: PlaybookSpec = vam_spec::encoding::json_to_proto(
            "vam.v1.PlaybookSpec",
            include_str!("../../../examples/pr-review/playbook.json"),
        )
        .expect("pr-review playbook decodes");
        let scenario: Scenario = serde_json::from_str(include_str!(
            "../../../examples/pr-review/playbook-scenario.json"
        ))
        .expect("pr-review scenario decodes");

        let report = run_playbook_test(&playbook, &scenario, 1).expect("runs");
        assert_eq!(report.passed, 1, "failures: {:?}", report.failures);
        assert_eq!(report.pass_k, 1.0);
    }

    #[test]
    fn repeat_outside_one_to_thirty_is_rejected() {
        let s = passing_evidence_scenario();
        assert!(run_playbook_test(&evidence_spec(), &s, 0).is_err());
        assert!(run_playbook_test(&evidence_spec(), &s, MAX_REPEAT + 1).is_err());
    }
}
