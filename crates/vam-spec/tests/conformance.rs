use prost::Message;
use prost_reflect::{DescriptorPool, Kind};
use std::fs;
use std::path::PathBuf;
use vam_spec::v1::{
    AgentSpec, CapabilityManifest, FileRef, PlaybookSpec, RunSpec, SignedEnvelope, SkillManifest,
};
use vam_spec::validation::Validate;

fn assert_kinds_compatible(left: &Kind, right: &Kind) {
    match (left, right) {
        (Kind::Message(l_msg), Kind::Message(r_msg)) => {
            assert_eq!(l_msg.full_name(), r_msg.full_name());
        }
        (Kind::Enum(l_enum), Kind::Enum(r_enum)) => {
            assert_eq!(l_enum.full_name(), r_enum.full_name());
        }
        (l, r) => {
            assert_eq!(l, r);
        }
    }
}

#[test]
fn test_protocol_compatibility() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let baseline_path = manifest_dir.join("../../proto/vam/v1/baseline/vam_descriptor.bin");
    let baseline_bytes = fs::read(baseline_path).expect("failed to read baseline descriptor");
    let baseline_pool =
        DescriptorPool::decode(&*baseline_bytes).expect("failed to decode baseline pool");

    let current_pool = &vam_spec::encoding::DESCRIPTOR_POOL;

    // Check message compatibility
    for baseline_msg in baseline_pool.all_messages() {
        let current_msg = current_pool
            .get_message_by_name(baseline_msg.full_name())
            .unwrap_or_else(|| panic!("Message '{}' has been deleted!", baseline_msg.full_name()));

        for baseline_field in baseline_msg.fields() {
            let current_field = current_msg
                .get_field(baseline_field.number())
                .unwrap_or_else(|| {
                    panic!(
                        "Field '{}' (tag {}) in message '{}' has been deleted or changed!",
                        baseline_field.name(),
                        baseline_field.number(),
                        baseline_msg.full_name()
                    )
                });

            assert_eq!(
                baseline_field.name(),
                current_field.name(),
                "Field name mismatch for tag {} in '{}'",
                baseline_field.number(),
                baseline_msg.full_name()
            );

            assert_kinds_compatible(&baseline_field.kind(), &current_field.kind());

            assert_eq!(
                baseline_field.cardinality(),
                current_field.cardinality(),
                "Field cardinality mismatch for field '{}' in '{}'",
                baseline_field.name(),
                baseline_msg.full_name()
            );
        }
    }

    // Check enum compatibility
    for baseline_enum in baseline_pool.all_enums() {
        let current_enum = current_pool
            .get_enum_by_name(baseline_enum.full_name())
            .unwrap_or_else(|| panic!("Enum '{}' has been deleted!", baseline_enum.full_name()));

        for baseline_value in baseline_enum.values() {
            let current_value = current_enum
                .get_value(baseline_value.number())
                .unwrap_or_else(|| {
                    panic!(
                        "Enum value '{}' (number {}) in enum '{}' has been deleted or changed!",
                        baseline_value.name(),
                        baseline_value.number(),
                        baseline_enum.full_name()
                    )
                });

            assert_eq!(
                baseline_value.name(),
                current_value.name(),
                "Enum value name mismatch for number {} in enum '{}'",
                baseline_value.number(),
                baseline_enum.full_name()
            );
        }
    }
}

#[test]
fn test_valid_fixtures_conformance() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let baseline_dir = manifest_dir.join("../../proto/vam/v1/baseline");

    // Helper to test round-trip of valid JSON/bin fixtures
    let test_valid_fixture = |proto_name: &str, name: &str| {
        let json_path = baseline_dir.join(format!("{}.json", name));
        let bin_path = baseline_dir.join(format!("{}.bin", name));

        let json_data = fs::read_to_string(&json_path).expect("failed to read json");
        let bin_data = fs::read(&bin_path).expect("failed to read bin");

        // Convert JSON to protobuf binary and verify against checked-in binary
        let desc = vam_spec::encoding::DESCRIPTOR_POOL
            .get_message_by_name(proto_name)
            .unwrap();
        let mut deserializer = serde_json::Deserializer::from_str(&json_data);
        let dynamic = prost_reflect::DynamicMessage::deserialize(desc, &mut deserializer).unwrap();

        let mut generated_bin = Vec::new();
        dynamic.encode(&mut generated_bin).unwrap();

        assert_eq!(
            generated_bin, bin_data,
            "Generated binary mismatch for fixture '{}'",
            name
        );

        // Perform semantic validation on the concrete types
        match proto_name {
            "vam.v1.AgentSpec" => {
                let msg = AgentSpec::decode(&*bin_data).unwrap();
                assert!(msg.validate().is_ok());
            }
            "vam.v1.SkillManifest" => {
                let msg = SkillManifest::decode(&*bin_data).unwrap();
                assert!(msg.validate().is_ok());
            }
            "vam.v1.PlaybookSpec" => {
                let msg = PlaybookSpec::decode(&*bin_data).unwrap();
                assert!(msg.validate().is_ok());
            }
            "vam.v1.RunSpec" => {
                let msg = RunSpec::decode(&*bin_data).unwrap();
                assert!(msg.validate().is_ok());
            }
            "vam.v1.SignedEnvelope" => {
                let msg = SignedEnvelope::decode(&*bin_data).unwrap();
                assert!(msg.validate().is_ok());
            }
            "vam.v1.CapabilityManifest" => {
                let msg = CapabilityManifest::decode(&*bin_data).unwrap();
                assert!(msg.validate().is_ok());
            }
            "vam.v1.FileRef" => {
                let msg = FileRef::decode(&*bin_data).unwrap();
                assert!(msg.validate().is_ok());
            }
            _ => panic!("untested message type: {}", proto_name),
        }
    };

    test_valid_fixture("vam.v1.AgentSpec", "agent_spec_valid");
    test_valid_fixture("vam.v1.SkillManifest", "skill_manifest_valid");
    test_valid_fixture("vam.v1.PlaybookSpec", "playbook_spec_valid");
    test_valid_fixture("vam.v1.RunSpec", "run_spec_valid");
    test_valid_fixture("vam.v1.SignedEnvelope", "signed_envelope_valid");
    test_valid_fixture("vam.v1.CapabilityManifest", "capability_manifest_valid");
    test_valid_fixture(
        "vam.v1.CapabilityManifest",
        "capability_manifest_run_request_valid",
    );
    test_valid_fixture("vam.v1.FileRef", "file_ref_valid");
}

#[test]
fn test_invalid_fixtures_conformance() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let baseline_dir = manifest_dir.join("../../proto/vam/v1/baseline");

    // Helper to test validation of invalid fixtures
    let test_invalid_fixture = |proto_name: &str, name: &str| {
        let bin_path = baseline_dir.join(format!("{}.bin", name));
        let bin_data = fs::read(&bin_path).expect("failed to read bin");

        // Perform semantic validation on the concrete types and expect error
        match proto_name {
            "vam.v1.AgentSpec" => {
                let msg = AgentSpec::decode(&*bin_data).unwrap();
                let res = msg.validate();
                assert!(
                    res.is_err(),
                    "Expected error for invalid agent spec: {}",
                    name
                );
                println!("AgentSpec validation error: {}", res.err().unwrap());
            }
            "vam.v1.SkillManifest" => {
                let msg = SkillManifest::decode(&*bin_data).unwrap();
                let res = msg.validate();
                assert!(
                    res.is_err(),
                    "Expected error for invalid skill manifest: {}",
                    name
                );
                println!("SkillManifest validation error: {}", res.err().unwrap());
            }
            "vam.v1.PlaybookSpec" => {
                let msg = PlaybookSpec::decode(&*bin_data).unwrap();
                let res = msg.validate();
                assert!(
                    res.is_err(),
                    "Expected error for invalid playbook spec: {}",
                    name
                );
                println!("PlaybookSpec validation error: {}", res.err().unwrap());
            }
            "vam.v1.SignedEnvelope" => {
                let msg = SignedEnvelope::decode(&*bin_data).unwrap();
                let res = msg.validate();
                assert!(
                    res.is_err(),
                    "Expected error for invalid signed envelope: {}",
                    name
                );
                println!("SignedEnvelope validation error: {}", res.err().unwrap());
            }
            "vam.v1.CapabilityManifest" => {
                let msg = CapabilityManifest::decode(&*bin_data).unwrap();
                let res = msg.validate();
                assert!(
                    res.is_err(),
                    "Expected error for invalid capability manifest: {}",
                    name
                );
                println!(
                    "CapabilityManifest validation error: {}",
                    res.err().unwrap()
                );
            }
            "vam.v1.FileRef" => {
                let msg = FileRef::decode(&*bin_data).unwrap();
                let res = msg.validate();
                assert!(
                    res.is_err(),
                    "Expected error for invalid file ref: {}",
                    name
                );
                println!("FileRef validation error: {}", res.err().unwrap());
            }
            _ => panic!("untested message type: {}", proto_name),
        }
    };

    test_invalid_fixture("vam.v1.AgentSpec", "agent_spec_invalid_id");
    test_invalid_fixture("vam.v1.SkillManifest", "skill_manifest_invalid_tools");
    test_invalid_fixture("vam.v1.PlaybookSpec", "playbook_spec_invalid_cycle");
    test_invalid_fixture("vam.v1.SignedEnvelope", "signed_envelope_invalid_signature");
    test_invalid_fixture(
        "vam.v1.CapabilityManifest",
        "capability_manifest_invalid_broker_id",
    );
    test_invalid_fixture(
        "vam.v1.CapabilityManifest",
        "capability_manifest_invalid_run_request",
    );
    test_invalid_fixture("vam.v1.FileRef", "file_ref_invalid_urn");
}

#[test]
fn jira_assistant_example_catalog_validates() {
    let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/jira-assistant");

    let playbook_raw =
        fs::read_to_string(examples.join("playbook.json")).expect("read playbook.json");
    let playbook: PlaybookSpec =
        vam_spec::encoding::json_to_proto("vam.v1.PlaybookSpec", &playbook_raw)
            .unwrap_or_else(|e| panic!("playbook.json deserialize: {e}"));
    playbook
        .validate()
        .unwrap_or_else(|e| panic!("playbook.json validate: {e}"));
    assert_eq!(
        playbook.resource_id.as_ref().map(|r| r.urn.as_str()),
        Some("urn:vam:playbook:local.vam.dev/vam/jira-sprint-backlog-cleanup")
    );

    let agent_raw = fs::read_to_string(examples.join("agent.json")).expect("read agent.json");
    let agent: AgentSpec = vam_spec::encoding::json_to_proto("vam.v1.AgentSpec", &agent_raw)
        .unwrap_or_else(|e| panic!("agent.json deserialize: {e}"));
    agent
        .validate()
        .unwrap_or_else(|e| panic!("agent.json validate: {e}"));

    let skill_raw = fs::read_to_string(examples.join("skills/jira-integration/manifest.json"))
        .expect("read skill manifest");
    let skill: SkillManifest =
        vam_spec::encoding::json_to_proto("vam.v1.SkillManifest", &skill_raw)
            .unwrap_or_else(|e| panic!("manifest.json deserialize: {e}"));
    skill
        .validate()
        .unwrap_or_else(|e| panic!("manifest.json validate: {e}"));
}

fn load_playbook(dir: &str) -> PlaybookSpec {
    let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let raw = fs::read_to_string(examples.join(dir).join("playbook.json"))
        .unwrap_or_else(|e| panic!("read {dir}/playbook.json: {e}"));
    let playbook: PlaybookSpec = vam_spec::encoding::json_to_proto("vam.v1.PlaybookSpec", &raw)
        .unwrap_or_else(|e| panic!("{dir}/playbook.json deserialize: {e}"));
    playbook
        .validate()
        .unwrap_or_else(|e| panic!("{dir}/playbook.json validate: {e}"));
    playbook
}

#[test]
fn pr_review_and_invoice_examples_validate_and_show_their_gate_shapes() {
    use vam_spec::v1::{OutcomeTerminal, StepKind};

    let pr_review = load_playbook("pr-review");
    assert_eq!(
        pr_review.resource_id.as_ref().map(|r| r.urn.as_str()),
        Some("urn:vam:playbook:local.vam.dev/vam/pr-review")
    );
    let approve = pr_review
        .steps
        .iter()
        .find(|s| s.id == "approve")
        .expect("pr-review has an approve gate");
    assert_eq!(approve.kind, StepKind::Gate as i32);
    assert!(
        approve.choices_from.is_empty(),
        "approval gate must not use choicesFrom"
    );
    let labels: Vec<&str> = approve.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, ["approved", "changes-requested"]);
    let terminal = approve
        .options
        .iter()
        .find(|o| o.label == "changes-requested")
        .map(|o| o.terminal);
    assert_eq!(terminal, Some(OutcomeTerminal::Succeeded as i32));
    assert_eq!(pr_review.guards.len(), 1);
    assert_eq!(pr_review.guards[0].allowed_when, ["approve:approved"]);

    let invoice = load_playbook("invoice-approval");
    let decide = invoice
        .steps
        .iter()
        .find(|s| s.id == "decide")
        .expect("invoice has a branch gate");
    assert_eq!(decide.kind, StepKind::Gate as i32);
    assert!(
        decide.choices_from.is_empty(),
        "branch gate must not use choicesFrom"
    );
    let labels: Vec<&str> = decide.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, ["pay", "reject"]);
    let reject = decide
        .options
        .iter()
        .find(|o| o.label == "reject")
        .map(|o| o.terminal);
    assert_eq!(reject, Some(OutcomeTerminal::Succeeded as i32));
    let payment = invoice
        .steps
        .iter()
        .find(|s| s.id == "schedule-payment")
        .expect("invoice has a payment step");
    assert_eq!(payment.after, ["decide:pay"]);
    assert_eq!(invoice.guards[0].allowed_when, ["decide:pay"]);
}
