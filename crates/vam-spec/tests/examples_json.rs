//! Every example JSON file that declares a VAM proto message must decode
//! through `vam_spec::encoding::json_to_proto`. Unknown fields are denied, so
//! a drifted example fails this test instead of teaching a shape the CLI
//! cannot load.

use std::fs;
use std::path::{Path, PathBuf};

use prost_reflect::{Cardinality, FieldDescriptor, Kind, MessageDescriptor};
use serde_json::Value;
use vam_spec::encoding::DESCRIPTOR_POOL;
use vam_spec::v1::{AgentSpec, CapabilityManifest, CreateRunRequest, PlaybookSpec, SkillManifest};
use vam_spec::validation::Validate;

/// Filename → fully qualified proto message. Ecosystem manifests (npm, tsconfig)
/// are not VAM messages and have nothing to decode.
fn declared_message(file_name: &str) -> Result<Option<&'static str>, String> {
    match file_name {
        "run.json" => Ok(Some("vam.v1.CreateRunRequest")),
        "agent.json" => Ok(Some("vam.v1.AgentSpec")),
        "playbook.json" => Ok(Some("vam.v1.PlaybookSpec")),
        "manifest.json" => Ok(Some("vam.v1.SkillManifest")),
        "put-broker.json" => Ok(Some("vam.v1.CapabilityManifest")),
        "package.json" | "package-lock.json" | "tsconfig.json" => Ok(None),
        // Scenario for `vam playbook test`: read by the test runner, not decoded as a proto.
        "playbook-scenario.json" => Ok(None),
        other => Err(format!(
            "no declared proto message for `{other}`; add a mapping or an explicit skip"
        )),
    }
}

fn decode_declared(proto_name: &str, json: &str) -> Result<(), String> {
    match proto_name {
        "vam.v1.CreateRunRequest" => {
            let _: CreateRunRequest = vam_spec::encoding::json_to_proto(proto_name, json)?;
        }
        "vam.v1.AgentSpec" => {
            let _: AgentSpec = vam_spec::encoding::json_to_proto(proto_name, json)?;
        }
        "vam.v1.PlaybookSpec" => {
            let _: PlaybookSpec = vam_spec::encoding::json_to_proto(proto_name, json)?;
        }
        "vam.v1.SkillManifest" => {
            let _: SkillManifest = vam_spec::encoding::json_to_proto(proto_name, json)?;
        }
        "vam.v1.CapabilityManifest" => {
            let _: CapabilityManifest = vam_spec::encoding::json_to_proto(proto_name, json)?;
        }
        other => return Err(format!("unhandled declared message `{other}`")),
    }
    Ok(())
}

fn collect_json(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("read dirent under {}: {e}", dir.display()));
        let path = entry.path();
        if path.is_dir() {
            collect_json(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            out.push(path);
        }
    }
}

#[test]
fn examples_json_decodes_as_declared_message() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let examples = repo.join("examples");
    let mut files = Vec::new();
    collect_json(&examples, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "expected JSON files under {}",
        examples.display()
    );

    let mut decoded = 0usize;
    let mut errors = Vec::new();
    for path in &files {
        let rel = path
            .strip_prefix(&repo)
            .unwrap_or(path)
            .display()
            .to_string();
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let proto_name = match declared_message(file_name) {
            Ok(Some(name)) => name,
            Ok(None) => continue,
            Err(e) => {
                errors.push(format!("{rel}: {e}"));
                continue;
            }
        };
        let json = match fs::read_to_string(path) {
            Ok(json) => json,
            Err(e) => {
                errors.push(format!("{rel}: read error: {e}"));
                continue;
            }
        };
        decoded += 1;
        if let Err(e) = decode_declared(proto_name, &json) {
            errors.push(format!("{rel} ({proto_name}): {e}"));
            continue;
        }
        if let Err(e) = assert_camel_case_proto_json(proto_name, &json) {
            errors.push(format!("{rel} ({proto_name}): {e}"));
        }
    }

    assert!(
        decoded > 0,
        "walker skipped every example JSON file; declared-message map is empty"
    );
    assert!(
        errors.is_empty(),
        "example JSON failed to decode:\n{}",
        errors.join("\n")
    );
}

/// Every example AgentSpec and PlaybookSpec must pass `validate()`, not just
/// decode. Decoding alone let examples drift into shapes the server rejects.
#[test]
fn examples_agent_and_playbook_validate() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let examples = repo.join("examples");
    let mut files = Vec::new();
    collect_json(&examples, &mut files);
    files.sort();

    let mut validated = 0usize;
    let mut errors = Vec::new();
    for path in &files {
        let rel = path
            .strip_prefix(&repo)
            .unwrap_or(path)
            .display()
            .to_string();
        let result = match path.file_name().and_then(|name| name.to_str()) {
            Some("agent.json") => validate_example::<AgentSpec>("vam.v1.AgentSpec", path),
            Some("playbook.json") => validate_example::<PlaybookSpec>("vam.v1.PlaybookSpec", path),
            _ => continue,
        };
        validated += 1;
        if let Err(e) = result {
            errors.push(format!("{rel}: {e}"));
        }
    }

    assert!(
        validated > 0,
        "found no agent.json or playbook.json under {}",
        examples.display()
    );
    assert!(
        errors.is_empty(),
        "example specs failed validate():\n{}",
        errors.join("\n")
    );
}

fn validate_example<T>(proto_name: &str, path: &Path) -> Result<(), String>
where
    T: prost::Message + Default,
    T: Validate,
{
    let json = fs::read_to_string(path).map_err(|e| format!("read error: {e}"))?;
    let spec: T = vam_spec::encoding::json_to_proto(proto_name, &json)?;
    spec.validate().map_err(|e| e.to_string())
}

/// Protobuf JSON field names are camelCase. Map keys and `google.protobuf.Struct`
/// values (run inputs, JSON Schema) are not proto fields and stay as written.
fn assert_camel_case_proto_json(proto_name: &str, json: &str) -> Result<(), String> {
    let message = DESCRIPTOR_POOL
        .get_message_by_name(proto_name)
        .ok_or_else(|| format!("message '{proto_name}' not found"))?;
    let value: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let mut snake = Vec::new();
    collect_snake_proto_fields(&value, &message, "$", &mut snake);
    if snake.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "snake_case protobuf JSON fields (use camelCase): {}",
            snake.join(", ")
        ))
    }
}

fn collect_snake_proto_fields(
    value: &Value,
    message: &MessageDescriptor,
    path: &str,
    out: &mut Vec<String>,
) {
    let Value::Object(map) = value else {
        return;
    };
    for (key, child) in map {
        let Some(field) = message
            .get_field_by_name(key)
            .or_else(|| message.get_field_by_json_name(key))
        else {
            continue;
        };
        if field.name() != field.json_name() && key == field.name() {
            out.push(format!("{path}.{key}"));
        }
        collect_snake_in_field(child, &field, &format!("{path}.{key}"), out);
    }
}

fn collect_snake_in_field(
    value: &Value,
    field: &FieldDescriptor,
    path: &str,
    out: &mut Vec<String>,
) {
    if field.is_map() {
        let Value::Object(map) = value else {
            return;
        };
        let Kind::Message(entry) = field.kind() else {
            return;
        };
        let value_field = entry.map_entry_value_field();
        for (key, child) in map {
            collect_snake_in_field(child, &value_field, &format!("{path}.{key}"), out);
        }
        return;
    }
    if field.cardinality() == Cardinality::Repeated {
        let Value::Array(items) = value else {
            return;
        };
        if let Kind::Message(message) = field.kind() {
            if is_opaque_well_known(&message) {
                return;
            }
            for (index, item) in items.iter().enumerate() {
                collect_snake_proto_fields(item, &message, &format!("{path}[{index}]"), out);
            }
        }
        return;
    }
    if let Kind::Message(message) = field.kind() {
        if is_opaque_well_known(&message) {
            return;
        }
        collect_snake_proto_fields(value, &message, path, out);
    }
}

fn is_opaque_well_known(message: &MessageDescriptor) -> bool {
    matches!(
        message.full_name(),
        "google.protobuf.Struct"
            | "google.protobuf.Value"
            | "google.protobuf.ListValue"
            | "google.protobuf.Any"
            | "google.protobuf.Timestamp"
            | "google.protobuf.Duration"
            | "google.protobuf.Empty"
            | "google.protobuf.FieldMask"
    )
}

#[test]
fn skill_manifest_capabilities_and_policy_round_trip_json_to_proto() {
    let json = r#"{
        "resourceId": {"urn": "urn:vam:skill:hub.vam.dev/acme/jira"},
        "version": {"major": 1, "minor": 0, "patch": 0},
        "description": "Jira",
        "capabilities": [
            "tool:jira.search",
            "egress:api.atlassian.com",
            "secret:jira/token",
            "fs:workspace:write"
        ],
        "policy": "permit (principal, action == Action::\"ExecuteTool\", resource == Tool::\"jira.search\");"
    }"#;

    let manifest: SkillManifest =
        vam_spec::encoding::json_to_proto("vam.v1.SkillManifest", json).expect("json_to_proto");
    assert_eq!(
        manifest.capabilities,
        vec![
            "tool:jira.search".to_string(),
            "egress:api.atlassian.com".to_string(),
            "secret:jira/token".to_string(),
            "fs:workspace:write".to_string(),
        ]
    );
    assert!(manifest.policy.contains("Tool::\"jira.search\""));
    manifest
        .validate()
        .expect("structured capabilities validate");

    let encoded = vam_spec::encoding::proto_to_json("vam.v1.SkillManifest", &manifest)
        .expect("proto_to_json");
    let again: SkillManifest =
        vam_spec::encoding::json_to_proto("vam.v1.SkillManifest", &encoded).expect("round trip");
    assert_eq!(again.capabilities, manifest.capabilities);
    assert_eq!(again.policy, manifest.policy);
}

#[test]
fn skill_manifest_playbooks_round_trip_json_to_proto() {
    let json = r#"{
        "resourceId": {"urn": "urn:vam:skill:hub.vam.dev/acme/jira"},
        "version": {"major": 1, "minor": 0, "patch": 0},
        "description": "Jira",
        "playbooks": ["./playbooks/sprint-cleanup.json"]
    }"#;

    let manifest: SkillManifest =
        vam_spec::encoding::json_to_proto("vam.v1.SkillManifest", json).expect("json_to_proto");
    assert_eq!(
        manifest.playbooks,
        vec!["./playbooks/sprint-cleanup.json".to_string()]
    );
    manifest.validate().expect("relative playbook path");

    let encoded = vam_spec::encoding::proto_to_json("vam.v1.SkillManifest", &manifest)
        .expect("proto_to_json");
    let again: SkillManifest =
        vam_spec::encoding::json_to_proto("vam.v1.SkillManifest", &encoded).expect("round trip");
    assert_eq!(again.playbooks, manifest.playbooks);
}

#[test]
fn playbook_declaring_two_skills_round_trips_and_resolves_both() {
    let json = r#"{
        "resourceId": {"urn": "urn:vam:playbook:hub.vam.dev/acme/sprint-cleanup"},
        "displayName": "Sprint cleanup",
        "description": "Search Jira, then post to Slack.",
        "steps": [{
            "id": "search",
            "instruction": "Search Jira for stale issues.",
            "mode": "STEP_MODE_AGENTIC"
        }],
        "requires": {
            "skills": [
                {"resourceId": {"urn": "urn:vam:skill:hub.vam.dev/acme/jira"}},
                {"resourceId": {"urn": "urn:vam:skill:hub.vam.dev/acme/slack"}}
            ],
            "capabilities": ["tool:jira.search", "tool:slack.post"]
        }
    }"#;

    let playbook: PlaybookSpec =
        vam_spec::encoding::json_to_proto("vam.v1.PlaybookSpec", json).expect("json_to_proto");
    playbook.validate().expect("two skill refs");
    let skills = vam_spec::required_skill_refs(&playbook);
    assert_eq!(skills.len(), 2);
    assert_eq!(
        skills[0].resource_id.as_ref().map(|id| id.urn.as_str()),
        Some("urn:vam:skill:hub.vam.dev/acme/jira")
    );
    assert_eq!(
        skills[1].resource_id.as_ref().map(|id| id.urn.as_str()),
        Some("urn:vam:skill:hub.vam.dev/acme/slack")
    );
    let requires = playbook.requires.as_ref().expect("requires");
    assert_eq!(
        requires.capabilities,
        vec![
            "tool:jira.search".to_string(),
            "tool:slack.post".to_string()
        ]
    );

    let encoded =
        vam_spec::encoding::proto_to_json("vam.v1.PlaybookSpec", &playbook).expect("proto_to_json");
    let again: PlaybookSpec =
        vam_spec::encoding::json_to_proto("vam.v1.PlaybookSpec", &encoded).expect("round trip");
    let again_skills = vam_spec::required_skill_refs(&again);
    assert_eq!(again_skills.len(), 2);
    assert_eq!(
        again_skills[0]
            .resource_id
            .as_ref()
            .map(|id| id.urn.as_str()),
        skills[0].resource_id.as_ref().map(|id| id.urn.as_str())
    );
    assert_eq!(
        again_skills[1]
            .resource_id
            .as_ref()
            .map(|id| id.urn.as_str()),
        skills[1].resource_id.as_ref().map(|id| id.urn.as_str())
    );
}

#[test]
fn agent_extends_overlay_round_trips_json_to_proto() {
    let json = r#"{
        "resourceId": {"urn": "urn:vam:agent:hub.vam.dev/acme/child"},
        "displayName": "Child",
        "role": "assistant",
        "extends": {
            "resourceId": {"urn": "urn:vam:agent:hub.vam.dev/acme/parent"},
            "version": {"major": 3, "minor": 2, "patch": 0},
            "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "ab"}
        },
        "overlay": {
            "instructions": {"append": "Reply in Hindi or English."},
            "skills": {
                "add": [{"resourceId": {"urn": "urn:vam:skill:hub.vam.dev/acme/kb"}}],
                "remove": [{"resourceId": {"urn": "urn:vam:skill:hub.vam.dev/acme/old"}}]
            },
            "capabilities": {
                "grant": ["egress:example.com"],
                "revoke": ["egress:amazonaws.com"]
            }
        }
    }"#;

    let agent: AgentSpec =
        vam_spec::encoding::json_to_proto("vam.v1.AgentSpec", json).expect("json_to_proto");
    agent.validate().expect("extending agent");
    let parent = agent.extends.as_ref().expect("extends");
    assert_eq!(
        parent.resource_id.as_ref().map(|id| id.urn.as_str()),
        Some("urn:vam:agent:hub.vam.dev/acme/parent")
    );
    assert_eq!(
        parent.version.as_ref().map(|version| version.major),
        Some(3)
    );
    let overlay = agent.overlay.as_ref().expect("overlay");
    match overlay
        .instructions
        .as_ref()
        .and_then(|instructions| instructions.change.as_ref())
    {
        Some(vam_spec::v1::instruction_overlay::Change::Append(text)) => {
            assert_eq!(text, "Reply in Hindi or English.");
        }
        other => panic!("expected append, got {other:?}"),
    }
    let skills = overlay.skills.as_ref().expect("skills");
    assert_eq!(skills.add.len(), 1);
    assert_eq!(skills.remove.len(), 1);
    let capabilities = overlay.capabilities.as_ref().expect("capabilities");
    assert_eq!(capabilities.grant, vec!["egress:example.com".to_string()]);
    assert_eq!(
        capabilities.revoke,
        vec!["egress:amazonaws.com".to_string()]
    );

    let encoded =
        vam_spec::encoding::proto_to_json("vam.v1.AgentSpec", &agent).expect("proto_to_json");
    let again: AgentSpec =
        vam_spec::encoding::json_to_proto("vam.v1.AgentSpec", &encoded).expect("round trip");
    assert_eq!(again.extends, agent.extends);
    assert_eq!(again.overlay, agent.overlay);
}

#[test]
fn skill_manifest_tool_ui_round_trips_json_to_proto() {
    let json = r#"{
        "resourceId": {"urn": "urn:vam:skill:hub.vam.dev/acme/mail"},
        "version": {"major": 1, "minor": 0, "patch": 0},
        "description": "Mail",
        "tools": [{
            "name": "mail_list",
            "description": "List mail",
            "inputSchemaJson": "{}",
            "outputSchemaJson": "{\"type\":\"object\"}",
            "ui": {"show": "mail_list", "ask": "mail_send"}
        }]
    }"#;
    let manifest: SkillManifest =
        vam_spec::encoding::json_to_proto("vam.v1.SkillManifest", json).expect("json_to_proto");
    let ui = manifest.tools[0].ui.clone().expect("ui");
    assert_eq!(
        (ui.show.as_str(), ui.ask.as_str()),
        ("mail_list", "mail_send")
    );
    let back =
        vam_spec::encoding::proto_to_json("vam.v1.SkillManifest", &manifest).expect("encode");
    assert!(
        back.contains("\"ui\":{\"show\":\"mail_list\",\"ask\":\"mail_send\"}"),
        "{back}"
    );
    let again: SkillManifest =
        vam_spec::encoding::json_to_proto("vam.v1.SkillManifest", &back).expect("decode");
    assert_eq!(manifest, again);
}

#[test]
fn skill_manifest_requires_runtime_round_trips_and_absent_stays_absent() {
    let with = r#"{
        "resourceId": {"urn": "urn:vam:skill:hub.vam.dev/acme/pandas"},
        "version": {"major": 1, "minor": 0, "patch": 0},
        "description": "Pandas",
        "requiresRuntime": {"python": ">=3.11", "git": "*"}
    }"#;
    let manifest: SkillManifest =
        vam_spec::encoding::json_to_proto("vam.v1.SkillManifest", with).expect("json_to_proto");
    assert_eq!(manifest.requires_runtime["python"], ">=3.11");
    manifest.validate().expect("valid requirements");
    let encoded = vam_spec::encoding::proto_to_json("vam.v1.SkillManifest", &manifest)
        .expect("proto_to_json");
    let again: SkillManifest =
        vam_spec::encoding::json_to_proto("vam.v1.SkillManifest", &encoded).expect("round trip");
    assert_eq!(again.requires_runtime, manifest.requires_runtime);

    // MB-1.4 acceptance: a manifest with no requiresRuntime serializes with no such key,
    // so canonical JSON (and every digest taken over it) is what it was before the field.
    let without = r#"{
        "resourceId": {"urn": "urn:vam:skill:hub.vam.dev/acme/pandas"},
        "version": {"major": 1, "minor": 0, "patch": 0},
        "description": "Pandas"
    }"#;
    let m: SkillManifest =
        vam_spec::encoding::json_to_proto("vam.v1.SkillManifest", without).expect("parse");
    assert!(m.requires_runtime.is_empty());
    let out = vam_spec::encoding::proto_to_json("vam.v1.SkillManifest", &m).expect("encode");
    assert!(!out.contains("requiresRuntime"), "{out}");
    // Explicitly empty is the same message, so it also encodes as absent.
    let empty: SkillManifest = vam_spec::encoding::json_to_proto(
        "vam.v1.SkillManifest",
        r#"{"resourceId":{"urn":"urn:vam:skill:hub.vam.dev/acme/pandas"},"version":{"major":1,"minor":0,"patch":0},"description":"Pandas","requiresRuntime":{}}"#,
    )
    .expect("parse");
    assert_eq!(
        vam_spec::encoding::proto_to_json("vam.v1.SkillManifest", &empty).expect("encode"),
        out
    );
}

#[test]
fn skill_manifest_requires_runtime_bad_key_or_value_names_the_field() {
    for (key, val) in [
        ("Python", ">=3"),
        ("python", ">3"),
        ("python", "3.11"),
        ("1x", "*"),
    ] {
        let json = format!(
            r#"{{"resourceId":{{"urn":"urn:vam:skill:hub.vam.dev/acme/pandas"}},"version":{{"major":1,"minor":0,"patch":0}},"description":"d","requiresRuntime":{{"{key}":"{val}"}}}}"#
        );
        let m: SkillManifest =
            vam_spec::encoding::json_to_proto("vam.v1.SkillManifest", &json).expect("parse");
        let err = m.validate().expect_err("refused").to_string();
        assert!(err.contains("requires_runtime["), "{key} {val}: {err}");
    }
}
