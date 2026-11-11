//! The playbooks in `examples/` that show playbooks in flight (decision 21) must
//! publish, and their scenarios must pass.

use vam_orchestrator::playbook_test::{run_playbook_test, Scenario};
use vam_orchestrator::users::LOCAL_USER_ID;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_spec::v1::PlaybookSpec;

const EXAMPLES: &[(&str, &str)] = &[
    (
        "oracle-purchase-requisition",
        include_str!("../../../examples/oracle-purchase-requisition/playbook.json"),
    ),
    (
        "expense-claim",
        include_str!("../../../examples/expense-claim/playbook.json"),
    ),
    (
        "support-refund",
        include_str!("../../../examples/support-refund/playbook.json"),
    ),
    (
        "release-notes-guide",
        include_str!("../../../examples/release-notes-guide/playbook.json"),
    ),
    (
        "vendor-onboarding",
        include_str!("../../../examples/vendor-onboarding/playbook.json"),
    ),
];

fn decode(json: &str) -> PlaybookSpec {
    vam_spec::encoding::json_to_proto("vam.v1.PlaybookSpec", json).expect("example decodes")
}

#[test]
fn every_example_validates_for_publish_and_publishes() {
    for (name, json) in EXAMPLES {
        let spec = decode(json);
        spec.validate_for_publish()
            .unwrap_or_else(|e| panic!("{name} does not validate for publish: {e}"));
        assert!(
            vam_spec::validation::playbook_enforcement_warnings(&spec).is_empty(),
            "{name} warns"
        );
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .put_playbook_with_owner(&spec, LOCAL_USER_ID)
            .unwrap_or_else(|e| panic!("{name} does not publish: {e}"));
    }
}

#[test]
fn the_release_notes_guide_is_guidance_only() {
    let spec = decode(EXAMPLES[3].1);
    assert!(spec.steps.is_empty());
    assert!(!spec.guidance.trim().is_empty());
}

#[test]
fn the_oracle_example_uses_each_in_flight_feature() {
    let spec = decode(EXAMPLES[0].1);
    let json = vam_spec::encoding::proto_to_json("vam.v1.PlaybookSpec", &spec).expect("json");
    for needle in [
        "vam-workspace-file",
        "\"format\": \"date\"",
        "choicesFrom",
        "{{steps.pick-supplier.output.supplier_id}}",
        "{{steps.extract-invoice.output.supplier}}",
    ] {
        assert!(
            json.replace("\":\"", "\": \"").contains(needle) || json.contains(needle),
            "missing {needle}"
        );
    }
}

fn run(example: usize, scenario: &str) {
    let spec = decode(EXAMPLES[example].1);
    let scenario: Scenario = serde_json::from_str(scenario).expect("scenario decodes");
    let report = run_playbook_test(&spec, &scenario, 1).expect("runs");
    assert_eq!(report.passed, 1, "failures: {:?}", report.failures);
}

#[test]
fn the_oracle_guided_scenario_passes() {
    run(
        0,
        include_str!("../../../examples/oracle-purchase-requisition/playbook-scenario.json"),
    );
}

#[test]
fn the_support_refund_not_eligible_scenario_passes() {
    run(
        2,
        include_str!("../../../examples/support-refund/playbook-scenario.json"),
    );
}

mod bench_script {
    use super::*;
    use vam_inference::mock_script::{plan_reply, MockScripts};
    use vam_orchestrator::run_context::render_playbook_status;
    use vam_orchestrator::workflow::{PlaybookStepRecord, PlaybookStepState};
    use vam_spec::v1::{ChatMessage, ToolSpec};

    fn message(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        }
    }

    fn record(id: &str, state: PlaybookStepState, outcome: &str) -> PlaybookStepRecord {
        PlaybookStepRecord {
            step_id: id.to_string(),
            state,
            outcome: outcome.to_string(),
            output_json: None,
            agent_summary: None,
            evidence_refs: vec![],
            started_at: None,
            finished_at: None,
            transition_revision: 1,
        }
    }

    fn tools() -> Vec<ToolSpec> {
        ["start_playbook", "run_step"]
            .iter()
            .map(|name| ToolSpec {
                name: (*name).to_string(),
                ..Default::default()
            })
            .collect()
    }

    /// The scripted model reads the status block the host really renders and
    /// picks the next step from it: a mock-script run needs nothing else.
    #[test]
    fn the_oracle_mock_script_follows_the_status_block() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/bench/mock-scripts");
        let scripts = MockScripts::load_dir(&dir).expect("bench scripts load");
        let script = scripts.get("bench-oracle-pr").expect("oracle script");
        let spec = decode(EXAMPLES[0].1);
        let nonce = vam_security::vendor_text::FenceNonce::from_bytes([1u8; 16]);
        let status =
            |records: &[PlaybookStepRecord]| render_playbook_status(&spec, records, None, &nonce);
        let call = |messages: &[ChatMessage]| {
            let plan = plan_reply(script, &tools(), messages).expect("a rule matches");
            plan.tool_call.map(|c| (c.name, c.arguments))
        };

        let first = vec![
            message("system", "mock-script:bench-oracle-pr"),
            message("user", "raise a PR e2e-step:start"),
        ];
        let (name, _) = call(&first).expect("starts the playbook");
        assert_eq!(name, "start_playbook");

        // After the supplier-name gate: find-suppliers is available.
        let resumed = status(&[
            record("mode", PlaybookStepState::Completed, "guided"),
            record("supplier-name", PlaybookStepState::Completed, "submitted"),
            record("find-suppliers", PlaybookStepState::Available, ""),
        ]);
        assert!(resumed.contains("Available now: find-suppliers"));
        let (name, args) = call(&[
            message("system", "mock-script:bench-oracle-pr"),
            message("user", &resumed),
        ])
        .expect("runs the search");
        assert_eq!(name, "run_step");
        assert!(args.contains("find-suppliers"));

        // After the confirm gate: create-pr is available.
        let confirmed = status(&[
            record("confirm", PlaybookStepState::Completed, "approve"),
            record("create-pr", PlaybookStepState::Available, ""),
        ]);
        let (name, args) = call(&[
            message("system", "mock-script:bench-oracle-pr"),
            message("user", &confirmed),
        ])
        .expect("creates");
        assert_eq!(name, "run_step");
        assert!(args.contains("create-pr"));

        // A gate is open and nothing is available: the model just waits.
        let waiting = status(&[record("pick-supplier", PlaybookStepState::Open, "")]);
        let plan = plan_reply(
            script,
            &tools(),
            &[
                message("system", "mock-script:bench-oracle-pr"),
                message("user", &waiting),
            ],
        )
        .expect("waits");
        assert!(plan.tool_call.is_none());
    }
}
