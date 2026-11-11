//! Tests for `run_context::resolve_harness`.

use super::*;
use serde_json::json;

fn spec_with(harness: Option<serde_json::Value>) -> RunSpec {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert(
        "prompt".to_string(),
        Value {
            kind: Some(Kind::StringValue("hi".into())),
        },
    );
    if let Some(h) = harness {
        fields.insert(
            "harness".to_string(),
            Value {
                kind: Some(Kind::StructValue(
                    vam_spec::harness::json_to_struct(&h).expect("object"),
                )),
            },
        );
    }
    RunSpec {
        inputs: Some(Struct { fields }),
        ..Default::default()
    }
}

/// The stored record, read back through the spec's JSON form as `gather` and
/// resume read it.
fn stored(spec: &RunSpec) -> serde_json::Value {
    let json = vam_spec::encoding::proto_to_json("vam.v1.RunSpec", spec).expect("to json");
    let back: RunSpec =
        vam_spec::encoding::json_to_proto("vam.v1.RunSpec", &json).expect("from json");
    match back.inputs.as_ref().and_then(|i| i.fields.get("harness")) {
        Some(Value {
            kind: Some(Kind::StructValue(s)),
        }) => vam_spec::harness::struct_to_json(s),
        other => panic!("harness record, got {other:?}"),
    }
}

#[test]
fn a_run_without_harness_gets_the_base_record() {
    let mut spec = spec_with(None);
    resolve_harness(&mut spec).expect("resolve");
    let rec = stored(&spec);
    assert_eq!(rec["profile"], "base");
    assert_eq!(rec["version"], "0.1.0");
    assert!(rec["digest"].as_str().unwrap().starts_with("sha256:"));
    assert!(rec.get("overrides").is_none());
    let read = vam_spec::harness::Harness::from_inputs(spec.inputs.as_ref()).expect("guest");
    assert_eq!(read, vam_spec::harness::Harness::default());
}

#[test]
fn an_override_is_recorded_and_reaches_the_guest_inputs() {
    let mut spec = spec_with(Some(json!({"loop": {"detect": true}})));
    resolve_harness(&mut spec).expect("resolve");
    let rec = stored(&spec);
    assert_eq!(rec["overrides"], json!({"loop": {"detect": true}}));
    assert_eq!(rec["switches"]["loop"]["detect"], true);
    let read = vam_spec::harness::Harness::from_inputs(spec.inputs.as_ref()).expect("guest");
    assert!(read.loop_.detect);
}

#[test]
fn harness_is_stripped_from_the_playbook_schema_check_but_not_from_the_run() {
    let mut spec = spec_with(Some(json!({"loop": {"detect": true}})));
    resolve_harness(&mut spec).expect("resolve");
    // A playbook schema that does not declare `harness`: the check input drops it.
    let checked = inputs_for_schema_check(None, spec.inputs.as_ref()).expect("some");
    assert!(!checked.fields.contains_key("harness"));
    // The run's own inputs, which the guest reads, keep it.
    assert!(spec.inputs.as_ref().unwrap().fields.contains_key("harness"));
}

#[test]
fn a_caller_cannot_send_a_prebuilt_record() {
    for body in [
        json!({"switches": {"loop": {"detect": true}}}),
        json!({"digest": "sha256:aa"}),
        json!({"profile": "frontier"}),
        json!({"version": "9.9.9"}),
    ] {
        let key = body.as_object().unwrap().keys().next().unwrap().clone();
        let mut spec = spec_with(Some(body));
        let err = resolve_harness(&mut spec).expect_err("refused");
        assert!(
            err.to_string().contains(&format!("'{key}'")),
            "{err} should name {key}"
        );
    }
}

#[test]
fn a_bad_switch_is_refused_naming_it() {
    let mut spec = spec_with(Some(json!({"tools": {"rank": true}})));
    let err = resolve_harness(&mut spec).expect_err("refused");
    assert!(matches!(err, OrchestratorError::InvalidContract(_)));
    assert!(err.to_string().contains("tools.rank"));
    assert!(err.to_string().contains("not built"));
}

#[test]
fn human_jig_is_accepted_and_reaches_the_guest_and_ask_policy_is_still_refused() {
    let mut spec = spec_with(Some(json!({"human": {"jig": true}})));
    resolve_harness(&mut spec).expect("human.jig is built");
    assert_eq!(stored(&spec)["overrides"], json!({"human": {"jig": true}}));
    let read = vam_spec::harness::Harness::from_inputs(spec.inputs.as_ref()).expect("guest");
    assert!(read.human.jig);

    let mut spec = spec_with(Some(json!({"human": {"askPolicy": "early"}})));
    let err = resolve_harness(&mut spec).expect_err("refused");
    assert!(err.to_string().contains("human.askPolicy"));
}

#[test]
fn a_harness_that_is_not_an_object_is_refused() {
    let mut spec = spec_with(None);
    spec.inputs.as_mut().unwrap().fields.insert(
        "harness".into(),
        Value {
            kind: Some(Kind::StringValue("on".into())),
        },
    );
    assert!(resolve_harness(&mut spec).is_err());
}

#[test]
fn a_stored_record_survives_a_resume_unchanged() {
    let mut spec = spec_with(Some(json!({"calls": {"repairBudget": 2}})));
    resolve_harness(&mut spec).expect("resolve");
    let first = stored(&spec);
    let json = vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &spec).expect("json");
    let resumed: RunSpec =
        vam_spec::encoding::json_to_proto("vam.v1.RunSpec", &json).expect("back");
    assert_eq!(stored(&resumed), first);
    let read = vam_spec::harness::Harness::from_inputs(resumed.inputs.as_ref()).expect("read");
    assert_eq!(read.calls.repair_budget, 2);
}

#[test]
fn a_named_profile_is_stored_with_its_values_and_its_own_digest() {
    let mut base = spec_with(None);
    resolve_harness(&mut base).expect("base");
    let mut spec = spec_with(Some(json!({"profile": "open-small"})));
    resolve_harness(&mut spec).expect("resolve");
    let rec = stored(&spec);
    assert_eq!(rec["profile"], "open-small");
    assert_eq!(rec["version"], "0.1.0");
    assert!(rec.get("overrides").is_none());
    assert_ne!(rec["digest"], stored(&base)["digest"]);
    let read = vam_spec::harness::Harness::from_inputs(spec.inputs.as_ref()).expect("guest");
    assert_eq!(read.calls.repair_budget, 2);
    assert!(read.loop_.detect);
}

#[test]
fn an_override_on_a_named_profile_is_recorded_against_that_profile() {
    let mut spec = spec_with(Some(
        json!({"profile": "open-small", "loop": {"detect": false}}),
    ));
    resolve_harness(&mut spec).expect("resolve");
    let rec = stored(&spec);
    assert_eq!(rec["profile"], "open-small");
    assert_eq!(rec["overrides"], json!({"loop": {"detect": false}}));
}

#[test]
fn an_unknown_profile_name_is_refused_naming_profile() {
    let mut spec = spec_with(Some(json!({"profile": "x"})));
    let err = resolve_harness(&mut spec).expect_err("refused");
    assert!(matches!(err, OrchestratorError::InvalidContract(_)));
    assert!(err.to_string().contains("'profile'"), "{err}");
}

#[test]
fn no_profile_name_means_base_never_a_pending_profile() {
    let mut spec = spec_with(Some(json!({"calls": {"repairBudget": 2}})));
    resolve_harness(&mut spec).expect("resolve");
    assert_eq!(stored(&spec)["profile"], "base");
}

#[test]
fn a_pending_catalog_profile_is_never_taken_implicitly() {
    // Both open-model profiles are pending today: every catalog model resolves to base.
    for model in [
        "zai/glm-5",
        "alibaba/qwen3.6-27b",
        "deepseek/deepseek-v4-pro",
        "mock-model",
    ] {
        assert_eq!(catalog_profile_for(model), None, "{model}");
    }
    assert_eq!(implicit_profile(Some("open-strong")), None);
    assert_eq!(implicit_profile(Some("no-such-profile")), None);
    assert_eq!(implicit_profile(None), None);
}

#[test]
fn a_profile_with_evidence_is_taken_when_the_caller_names_none() {
    assert_eq!(implicit_profile(Some("base")).as_deref(), Some("base"));
    let mut spec = spec_with(None);
    default_harness_profile(&mut spec, Some("base"));
    resolve_harness(&mut spec).expect("resolve");
    assert_eq!(stored(&spec)["profile"], "base");
}

#[test]
fn the_callers_profile_wins_over_the_catalog_profile() {
    let mut spec = spec_with(Some(json!({"profile": "open-small"})));
    default_harness_profile(&mut spec, Some("base"));
    resolve_harness(&mut spec).expect("resolve");
    assert_eq!(stored(&spec)["profile"], "open-small");
}

#[test]
fn no_catalog_profile_leaves_the_inputs_alone() {
    let mut spec = spec_with(None);
    let before = spec.clone();
    default_harness_profile(&mut spec, None);
    assert_eq!(spec, before);
}
