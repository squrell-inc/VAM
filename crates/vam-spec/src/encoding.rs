use once_cell::sync::Lazy;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, Kind, MessageDescriptor};
use serde::Serialize;
use serde_json::{Deserializer, Serializer, Value};
use std::borrow::Cow;

use crate::v1::SignedEnvelope;

pub const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/vam_descriptor.bin"));

pub static DESCRIPTOR_POOL: Lazy<DescriptorPool> = Lazy::new(|| {
    DescriptorPool::decode(FILE_DESCRIPTOR_SET).expect("failed to decode descriptor pool")
});

/// Convert any prost message to a canonical JSON string using prost-reflect
pub fn proto_to_json<T: Message>(proto_name: &str, msg: &T) -> Result<String, String> {
    let desc = DESCRIPTOR_POOL
        .get_message_by_name(proto_name)
        .ok_or_else(|| format!("message '{}' not found in descriptor pool", proto_name))?;

    let mut buf = Vec::new();
    msg.encode(&mut buf)
        .map_err(|e| format!("failed to encode message: {}", e))?;

    let dynamic = DynamicMessage::decode(desc, &*buf)
        .map_err(|e| format!("failed to decode dynamic message: {}", e))?;

    let mut out = Vec::new();
    let mut serializer = Serializer::new(&mut out);
    dynamic
        .serialize(&mut serializer)
        .map_err(|e| format!("failed to serialize JSON: {}", e))?;

    let json =
        String::from_utf8(out).map_err(|e| format!("invalid UTF-8 in serialized JSON: {}", e))?;
    Ok(shorten_permission_enum_json(json))
}

/// Convert JSON string back to a concrete prost message
pub fn json_to_proto<T: Message + Default>(proto_name: &str, json: &str) -> Result<T, String> {
    let desc = DESCRIPTOR_POOL
        .get_message_by_name(proto_name)
        .ok_or_else(|| format!("message '{}' not found in descriptor pool", proto_name))?;

    let json = expand_permission_enum_json(&desc, json)?;
    let mut deserializer = Deserializer::from_str(&json);
    let dynamic = DynamicMessage::deserialize(desc, &mut deserializer)
        .map_err(|e| format!("failed to deserialize JSON: {}", e))?;

    deserializer
        .end()
        .map_err(|e| format!("trailing JSON data: {}", e))?;

    let mut buf = Vec::new();
    dynamic
        .encode(&mut buf)
        .map_err(|e| format!("failed to encode dynamic message: {}", e))?;

    let concrete =
        T::decode(&*buf).map_err(|e| format!("failed to decode concrete message: {}", e))?;

    Ok(concrete)
}

/// Author JSON for permission enums is design §IV.2 (`bySideEffect`, `ask`).
/// Proto value names stay prefixed (`TOOL_DEFAULT_BY_SIDE_EFFECT`) because
/// proto3 enum values share the package namespace. Existing enums are unchanged.
fn shorten_permission_enum_json(json: String) -> String {
    const PAIRS: &[(&str, &str)] = &[
        ("\"TOOL_DEFAULT_BY_SIDE_EFFECT\"", "\"bySideEffect\""),
        ("\"TOOL_DEFAULT_ALLOW\"", "\"allow\""),
        ("\"TOOL_DEFAULT_ASK\"", "\"ask\""),
        ("\"TOOL_DEFAULT_DENY\"", "\"deny\""),
        ("\"HOST_DEFAULT_ALLOW\"", "\"allow\""),
        ("\"HOST_DEFAULT_DENY\"", "\"deny\""),
        ("\"PERMISSION_DECISION_ALLOW\"", "\"allow\""),
        ("\"PERMISSION_DECISION_ASK\"", "\"ask\""),
        ("\"PERMISSION_DECISION_DENY\"", "\"deny\""),
        ("\"PERMISSION_CHANGE_KIND_LOOSENED\"", "\"loosened\""),
        ("\"PERMISSION_CHANGE_KIND_TIGHTENED\"", "\"tightened\""),
        ("\"PERMISSION_CHANGE_KIND_ADDED\"", "\"added\""),
        ("\"PERMISSION_CHANGE_KIND_REMOVED\"", "\"removed\""),
    ];
    if !json.contains("TOOL_DEFAULT_")
        && !json.contains("HOST_DEFAULT_")
        && !json.contains("PERMISSION_DECISION_")
        && !json.contains("PERMISSION_CHANGE_KIND_")
    {
        return json;
    }
    let mut out = json;
    for (from, to) in PAIRS {
        if out.contains(from) {
            out = out.replace(from, to);
        }
    }
    out
}

/// Accept the §IV.2 spelling on the way in. Prefixed proto names still decode.
/// The original text is kept when nothing changes, so other messages stay byte-identical.
fn expand_permission_enum_json<'a>(
    desc: &MessageDescriptor,
    json: &'a str,
) -> Result<Cow<'a, str>, String> {
    if !json.contains("\"bySideEffect\"")
        && !json.contains("\"allow\"")
        && !json.contains("\"ask\"")
        && !json.contains("\"deny\"")
        && !json.contains("\"loosened\"")
        && !json.contains("\"tightened\"")
        && !json.contains("\"added\"")
        && !json.contains("\"removed\"")
        && !json.contains("\"SAME\"")
        && !json.contains("\"NEW\"")
        && !json.contains("\"ID\"")
        && !json.contains("\"QUEUE\"")
        && !json.contains("\"SKIP\"")
    {
        return Ok(Cow::Borrowed(json));
    }
    let mut value: Value =
        serde_json::from_str(json).map_err(|e| format!("failed to parse JSON: {}", e))?;
    if !expand_permission_enums(desc, &mut value) {
        return Ok(Cow::Borrowed(json));
    }
    serde_json::to_string(&value)
        .map(Cow::Owned)
        .map_err(|e| format!("failed to rewrite JSON: {}", e))
}

fn expand_permission_enums(desc: &MessageDescriptor, value: &mut Value) -> bool {
    let Value::Object(map) = value else {
        return false;
    };
    let mut changed = false;
    for field in desc.fields() {
        if field.is_map() {
            continue;
        }
        let kind = field.kind();
        let key = field.json_name().to_string();
        let repeated = field.is_list();
        let Some(slot) = map.get_mut(&key) else {
            continue;
        };
        changed |= match kind {
            Kind::Enum(enum_desc) => {
                if repeated {
                    expand_enum_list(enum_desc.full_name(), slot)
                } else {
                    expand_enum_value(enum_desc.full_name(), slot)
                }
            }
            Kind::Message(message_desc) => {
                if repeated {
                    let Value::Array(items) = slot else {
                        continue;
                    };
                    let mut nested = false;
                    for item in items {
                        nested |= expand_permission_enums(&message_desc, item);
                    }
                    nested
                } else {
                    expand_permission_enums(&message_desc, slot)
                }
            }
            _ => false,
        };
    }
    changed
}

fn expand_enum_list(enum_name: &str, slot: &mut Value) -> bool {
    let Value::Array(items) = slot else {
        return false;
    };
    let mut changed = false;
    for item in items {
        changed |= expand_enum_value(enum_name, item);
    }
    changed
}

fn expand_enum_value(enum_name: &str, slot: &mut Value) -> bool {
    let Value::String(token) = slot else {
        return false;
    };
    let Some(proto_name) = permission_enum_proto_name(enum_name, token) else {
        return false;
    };
    *token = proto_name.to_string();
    true
}

fn permission_enum_proto_name(enum_name: &str, token: &str) -> Option<&'static str> {
    match (enum_name, token) {
        ("vam.v1.ToolDefault", "allow") => Some("TOOL_DEFAULT_ALLOW"),
        ("vam.v1.ToolDefault", "ask") => Some("TOOL_DEFAULT_ASK"),
        ("vam.v1.ToolDefault", "deny") => Some("TOOL_DEFAULT_DENY"),
        ("vam.v1.ToolDefault", "bySideEffect") => Some("TOOL_DEFAULT_BY_SIDE_EFFECT"),
        ("vam.v1.HostDefault", "allow") => Some("HOST_DEFAULT_ALLOW"),
        ("vam.v1.HostDefault", "deny") => Some("HOST_DEFAULT_DENY"),
        ("vam.v1.PermissionDecision", "allow") => Some("PERMISSION_DECISION_ALLOW"),
        ("vam.v1.PermissionDecision", "ask") => Some("PERMISSION_DECISION_ASK"),
        ("vam.v1.PermissionDecision", "deny") => Some("PERMISSION_DECISION_DENY"),
        ("vam.v1.PermissionChangeKind", "loosened") => Some("PERMISSION_CHANGE_KIND_LOOSENED"),
        ("vam.v1.PermissionChangeKind", "tightened") => Some("PERMISSION_CHANGE_KIND_TIGHTENED"),
        ("vam.v1.PermissionChangeKind", "added") => Some("PERMISSION_CHANGE_KIND_ADDED"),
        ("vam.v1.PermissionChangeKind", "removed") => Some("PERMISSION_CHANGE_KIND_REMOVED"),
        // Run grants: the schedule broker sends the short spelling (Q5).
        ("vam.v1.PlaceMode", "SAME") => Some("PLACE_MODE_SAME"),
        ("vam.v1.PlaceMode", "NEW") => Some("PLACE_MODE_NEW"),
        ("vam.v1.PlaceMode", "ID") => Some("PLACE_MODE_ID"),
        ("vam.v1.OnBusy", "QUEUE") => Some("ON_BUSY_QUEUE"),
        ("vam.v1.OnBusy", "SKIP") => Some("ON_BUSY_SKIP"),
        _ => None,
    }
}

/// Verify signed envelope domain and version before returning the deserialized claims
pub fn verify_envelope_claims<T: Message + Default>(
    envelope: &SignedEnvelope,
    expected_domain: &str,
    expected_major_version: i32,
) -> Result<T, String> {
    if envelope.domain != expected_domain {
        return Err(format!(
            "domain mismatch: expected '{}', got '{}'",
            expected_domain, envelope.domain
        ));
    }

    let version = envelope
        .envelope_version
        .as_ref()
        .ok_or_else(|| "missing envelope version".to_string())?;

    if version.major != expected_major_version {
        return Err(format!(
            "incompatible major version: expected major '{}', got '{}'",
            expected_major_version, version.major
        ));
    }

    T::decode(&*envelope.claims).map_err(|e| format!("failed to decode claims: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::{ResourceId, Signature, Version};

    #[test]
    fn test_json_roundtrip() {
        let original = ResourceId {
            urn: "urn:vam:agent:hub.vam.dev/acme/oracle-developer".to_string(),
        };
        let json = proto_to_json("vam.v1.ResourceId", &original).unwrap();
        assert!(json.contains("urn:vam:agent:hub.vam.dev/acme/oracle-developer"));

        let deserialized: ResourceId = json_to_proto("vam.v1.ResourceId", &json).unwrap();
        assert_eq!(original.urn, deserialized.urn);
    }

    #[test]
    fn playbook_step_prose_and_binding_round_trips() {
        use crate::v1::{PlaybookStep, StepBinding, StepMode};
        use prost_types::{value::Kind, Struct, Value};

        let mut arguments = Struct::default();
        arguments.fields.insert(
            "path".to_string(),
            Value {
                kind: Some(Kind::StringValue(
                    "/mnt/drive9/workspace/config.yaml".to_string(),
                )),
            },
        );
        let original = PlaybookStep {
            id: "read-config".to_string(),
            description: "Read the workspace config".to_string(),
            instruction: "Read config.yaml before continuing.".to_string(),
            binding: Some(StepBinding {
                tool_name: "read_file".to_string(),
                arguments: Some(arguments),
            }),
            mode: StepMode::Bound as i32,
            ..Default::default()
        };

        let json = proto_to_json("vam.v1.PlaybookStep", &original).unwrap();
        let decoded: PlaybookStep = json_to_proto("vam.v1.PlaybookStep", &json).unwrap();
        assert_eq!(decoded.instruction, original.instruction);
        let binding = decoded.binding.expect("binding round-trips");
        assert_eq!(binding.tool_name, "read_file");
        let args = binding.arguments.expect("arguments round-trip");
        assert_eq!(
            args.fields.get("path").and_then(|value| value.kind.clone()),
            Some(Kind::StringValue(
                "/mnt/drive9/workspace/config.yaml".to_string()
            ))
        );
        assert_eq!(decoded.mode, StepMode::Bound as i32);
    }

    /// Design §IV.2 author JSON. Proto3 JSON writes the enum value name, so the
    /// author-facing values are `bySideEffect`, `allow`, `ask`, and `deny`.
    #[test]
    fn agent_permissions_section_iv_2_round_trips() {
        use crate::v1::{
            AgentPermissions, HostDefault, PermissionDecision, PermissionRule, ToolDefault,
        };

        let json = r#"{
          "tools": {
            "default": "bySideEffect",
            "rules": [
              { "target": "skill:jira/create_issue", "decision": "ask" },
              { "target": "mcp:mcp.atlassian.com/*", "decision": "allow" },
              { "target": "builtin:run", "decision": "deny" }
            ]
          },
          "internet": {
            "default": "allow",
            "rules": [ { "target": "*.pastebin.com", "decision": "deny" } ]
          },
          "advanced": []
        }"#;

        let decoded: AgentPermissions =
            json_to_proto("vam.v1.AgentPermissions", json).expect("decode §IV.2");
        let tools = decoded.tools.clone().expect("tools");
        assert_eq!(tools.default, ToolDefault::BySideEffect as i32);
        assert_eq!(
            tools.rules,
            vec![
                PermissionRule {
                    target: "skill:jira/create_issue".to_string(),
                    decision: PermissionDecision::Ask as i32,
                },
                PermissionRule {
                    target: "mcp:mcp.atlassian.com/*".to_string(),
                    decision: PermissionDecision::Allow as i32,
                },
                PermissionRule {
                    target: "builtin:run".to_string(),
                    decision: PermissionDecision::Deny as i32,
                },
            ]
        );
        let internet = decoded.internet.clone().expect("internet");
        assert_eq!(internet.default, HostDefault::Allow as i32);
        assert_eq!(
            internet.rules,
            vec![PermissionRule {
                target: "*.pastebin.com".to_string(),
                decision: PermissionDecision::Deny as i32,
            }]
        );
        assert!(decoded.advanced.is_empty());

        let encoded = proto_to_json("vam.v1.AgentPermissions", &decoded).expect("encode");
        assert!(
            encoded.contains("\"default\":\"bySideEffect\""),
            "tools default must be the §IV.2 spelling, got {encoded}"
        );
        assert!(
            encoded.contains("\"decision\":\"ask\""),
            "decision must be the §IV.2 spelling, got {encoded}"
        );
        let again: AgentPermissions =
            json_to_proto("vam.v1.AgentPermissions", &encoded).expect("redecode");
        assert_eq!(decoded, again);

        let prefixed = encoded
            .replace("\"bySideEffect\"", "\"TOOL_DEFAULT_BY_SIDE_EFFECT\"")
            .replace("\"ask\"", "\"PERMISSION_DECISION_ASK\"");
        let from_prefixed: AgentPermissions = json_to_proto("vam.v1.AgentPermissions", &prefixed)
            .expect("prefixed names still decode");
        assert_eq!(decoded.tools, from_prefixed.tools);
    }

    #[test]
    fn agent_permissions_web_round_trips_and_absent_means_off() {
        use crate::v1::AgentPermissions;

        let on: AgentPermissions =
            json_to_proto("vam.v1.AgentPermissions", r#"{"web": {"enabled": true}}"#)
                .expect("decode web");
        assert!(on.web.as_ref().expect("web present").enabled);
        let encoded = proto_to_json("vam.v1.AgentPermissions", &on).expect("encode");
        assert!(encoded.contains("\"web\""), "got {encoded}");
        let again: AgentPermissions =
            json_to_proto("vam.v1.AgentPermissions", &encoded).expect("redecode");
        assert_eq!(on, again);

        let old: AgentPermissions = json_to_proto(
            "vam.v1.AgentPermissions",
            r#"{"tools": {"default": "bySideEffect"}}"#,
        )
        .expect("old json still loads");
        assert!(old.web.is_none());
    }

    #[test]
    fn hosted_services_new_fields_round_trip() {
        use crate::v1::{
            InvokeResponse, ResultSource, ResultSourceKind, ToolDestination, ToolExport,
        };
        use prost::Message;

        let export = ToolExport {
            tool_name: "web_fetch".to_string(),
            destination_policy: ToolDestination::PublicWeb as i32,
            effect: None,
            ..Default::default()
        };
        let json = proto_to_json("vam.v1.ToolExport", &export).expect("encode");
        let back: ToolExport = json_to_proto("vam.v1.ToolExport", &json).expect("decode");
        assert_eq!(export, back);
        // Old JSON without the field is the allowlist default.
        let old: ToolExport =
            json_to_proto("vam.v1.ToolExport", r#"{"toolName": "x"}"#).expect("old json");
        assert_eq!(old.destination_policy, ToolDestination::Allowlist as i32);

        let response = InvokeResponse {
            sources: vec![ResultSource {
                url: "https://a.example/".to_string(),
                title: "A".to_string(),
                published: "2026-01-01".to_string(),
                kind: ResultSourceKind::Page as i32,
            }],
            ..Default::default()
        };
        let decoded = InvokeResponse::decode(response.encode_to_vec().as_slice()).expect("wire");
        assert_eq!(response, decoded);
        let json = proto_to_json("vam.v1.InvokeResponse", &response).expect("encode");
        let back: InvokeResponse = json_to_proto("vam.v1.InvokeResponse", &json).expect("decode");
        assert_eq!(response, back);
    }

    #[test]
    fn tool_spec_approval_required_serializes_as_approval_required() {
        use crate::v1::ToolSpec;

        let spec = ToolSpec {
            name: "create_issue".to_string(),
            approval_required: true,
            ..Default::default()
        };
        let json = proto_to_json("vam.v1.ToolSpec", &spec).expect("encode");
        assert!(
            json.contains("\"approvalRequired\":true"),
            "JSON name must be approvalRequired, got {json}"
        );
        let decoded: ToolSpec = json_to_proto("vam.v1.ToolSpec", &json).expect("decode");
        assert!(decoded.approval_required);
    }

    #[test]
    fn tool_ui_and_approval_card_json_names() {
        use crate::v1::{ApprovalCard, ApprovalRequestClaims, ToolSpec, ToolUi};

        let spec = ToolSpec {
            name: "mail_list".to_string(),
            ui: Some(ToolUi {
                show: "list".into(),
                ask: "ask".into(),
            }),
            ..Default::default()
        };
        let json = proto_to_json("vam.v1.ToolSpec", &spec).expect("encode");
        assert!(
            json.contains("\"ui\":{\"show\":\"list\",\"ask\":\"ask\"}"),
            "{json}"
        );
        let back: ToolSpec = json_to_proto("vam.v1.ToolSpec", &json).expect("decode");
        assert_eq!(spec, back);

        let claims = ApprovalRequestClaims {
            card: Some(ApprovalCard {
                jig: "s.send".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let json = proto_to_json("vam.v1.ApprovalRequestClaims", &claims).expect("encode");
        assert!(json.contains("\"card\":{\"jig\":\"s.send\""), "{json}");
        let back: ApprovalRequestClaims =
            json_to_proto("vam.v1.ApprovalRequestClaims", &json).expect("decode");
        assert_eq!(claims, back);
    }

    #[test]
    fn permission_change_kind_json_is_the_short_word() {
        use crate::v1::{PermissionChange, PermissionChangeKind, ReleaseSuggestion};

        let msg = ReleaseSuggestion {
            permission_changes: vec![PermissionChange {
                target: "skill:jira/create_issue".to_string(),
                before: "ask".to_string(),
                after: "allow".to_string(),
                change: PermissionChangeKind::Loosened as i32,
                sentence: "Now allowed without asking: `skill:jira/create_issue` (was: ask)"
                    .to_string(),
            }],
            ..Default::default()
        };
        let json = proto_to_json("vam.v1.ReleaseSuggestion", &msg).expect("encode");
        assert!(
            json.contains("\"permissionChanges\""),
            "JSON name must be permissionChanges, got {json}"
        );
        assert!(
            json.contains("\"change\":\"loosened\""),
            "change must be the short word, got {json}"
        );
        assert!(
            !json.contains("PERMISSION_CHANGE_KIND"),
            "prefixed enum name must be shortened, got {json}"
        );
        let back: ReleaseSuggestion =
            json_to_proto("vam.v1.ReleaseSuggestion", &json).expect("decode");
        assert_eq!(back.permission_changes, msg.permission_changes);
    }

    #[test]
    fn test_signed_envelope_verification() {
        let claims = Version {
            major: 1,
            minor: 2,
            patch: 3,
            prerelease: "rc.1".to_string(),
            build: "build.456".to_string(),
        };
        let mut claims_bytes = Vec::new();
        claims.encode(&mut claims_bytes).unwrap();

        let envelope = SignedEnvelope {
            domain: "vam.v1.test-claims".to_string(),
            envelope_version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            claims: claims_bytes,
            signature: Some(Signature {
                algorithm: 1,
                key_id: "test-key".to_string(),
                value: vec![0, 1, 2, 3],
            }),
        };

        // Successful verification
        let verified: Version = verify_envelope_claims(&envelope, "vam.v1.test-claims", 1).unwrap();
        assert_eq!(verified.major, 1);
        assert_eq!(verified.minor, 2);
        assert_eq!(verified.patch, 3);
        assert_eq!(verified.prerelease, "rc.1");

        // Domain mismatch
        assert!(verify_envelope_claims::<Version>(&envelope, "wrong-domain", 1).is_err());

        // Major version mismatch
        assert!(verify_envelope_claims::<Version>(&envelope, "vam.v1.test-claims", 2).is_err());
    }

    #[test]
    fn run_grant_numbers_are_locked() {
        use crate::v1::{CallerContext, CapabilityManifest, InvokeRequest, RunSpec, RunStatus};
        let field = |msg: &str, name: &str| {
            DESCRIPTOR_POOL
                .get_message_by_name(msg)
                .and_then(|m| m.get_field_by_name(name))
                .map(|f| f.number())
        };
        let _ = (
            CapabilityManifest::default(),
            InvokeRequest::default(),
            CallerContext::default(),
            RunSpec::default(),
            RunStatus::default(),
        );
        assert_eq!(
            field("vam.v1.CapabilityManifest", "run_request_permission"),
            Some(10)
        );
        assert_eq!(
            field("vam.v1.CapabilityManifest", "notification_channel"),
            Some(11)
        );
        assert_eq!(field("vam.v1.InvokeRequest", "run_grant"), Some(9));
        assert_eq!(field("vam.v1.CallerContext", "space_id"), Some(8));
        assert_eq!(field("vam.v1.CallerContext", "thread_id"), Some(9));
        assert_eq!(field("vam.v1.CallerContext", "run_source"), Some(10));
        assert_eq!(field("vam.v1.RunSpec", "space_id"), Some(12));
        assert_eq!(field("vam.v1.RunSpec", "thread_id"), Some(13));
        assert_eq!(field("vam.v1.RunStatus", "space_id"), Some(20));
        assert_eq!(field("vam.v1.RunStatus", "thread_id"), Some(21));
        assert_eq!(field("vam.v1.RunStatus", "source"), Some(22));
    }

    #[test]
    fn effect_journal_numbers_are_locked() {
        let field = |msg: &str, name: &str| {
            DESCRIPTOR_POOL
                .get_message_by_name(msg)
                .and_then(|m| m.get_field_by_name(name))
                .map(|f| f.number())
        };
        assert_eq!(field("vam.v1.ToolIntentRequest", "call_id"), Some(6));
        assert_eq!(field("vam.v1.ExecuteHostToolRequest", "call_id"), Some(5));
        assert_eq!(field("vam.v1.ToolSpec", "effect"), Some(13));
        assert_eq!(field("vam.v1.ToolExport", "effect"), Some(5));
        assert_eq!(field("vam.v1.InvokeRequest", "idempotency_key"), Some(10));
        assert_eq!(field("vam.v1.RunStatus", "resumes"), Some(23));
        let reauth = DESCRIPTOR_POOL
            .get_enum_by_name("vam.v1.WaitingFor")
            .and_then(|e| e.get_value_by_name("WAITING_FOR_REAUTH"))
            .map(|v| v.number());
        assert_eq!(reauth, Some(4));
    }

    #[test]
    fn effect_fields_round_trip_in_json() {
        use crate::v1::{effect_found, CapabilityManifest, EffectClass, ToolSpec};
        let json = r#"{"brokerId":"effects-test-broker","trustTier":"TRUST_TIER_FIRST_PARTY",
            "tools":[{"toolName":"create","sideEffect":"SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL",
              "effect":{"class":"EFFECT_CLASS_NON_IDEMPOTENT",
                "verify":{"tool":"find","argsTemplate":{"title":"{{args.title}}"},
                          "found":{"path":"found","nonEmpty":true}}}},
              {"toolName":"find","sideEffect":"SIDE_EFFECT_CLASS_READ_ONLY"}]}"#;
        let manifest: CapabilityManifest =
            json_to_proto("vam.v1.CapabilityManifest", json).expect("decode");
        let effect = manifest.tools[0].effect.clone().expect("effect");
        assert_eq!(effect.class, EffectClass::NonIdempotent as i32);
        let verify = effect.verify.clone().expect("verify");
        assert_eq!(verify.tool, "find");
        assert_eq!(
            verify.found.unwrap().r#match,
            Some(effect_found::Match::NonEmpty(true))
        );
        assert!(manifest.tools[1].effect.is_none());
        let back = proto_to_json("vam.v1.CapabilityManifest", &manifest).expect("encode");
        let again: CapabilityManifest =
            json_to_proto("vam.v1.CapabilityManifest", &back).expect("redecode");
        assert_eq!(manifest, again);

        // `equals` is the other match; a skill tool carries the same message.
        let tool: ToolSpec = json_to_proto(
            "vam.v1.ToolSpec",
            r#"{"name":"mk","effect":{"class":"EFFECT_CLASS_IDEMPOTENT","verify":{"tool":"get",
               "found":{"path":"id","equals":"{{args.id}}"}}}}"#,
        )
        .expect("tool decode");
        let found = tool.effect.unwrap().verify.unwrap().found.unwrap();
        assert_eq!(
            found.r#match,
            Some(effect_found::Match::Equals("{{args.id}}".into()))
        );
    }

    #[test]
    fn effect_fields_survive_the_binary_round_trip() {
        use crate::v1::{
            EffectSpec, ExecuteHostToolRequest, InvokeRequest, RunStatus, ToolIntentRequest,
        };
        use prost::Message;
        let intent = ToolIntentRequest {
            call_id: "toolu_1".into(),
            ..Default::default()
        };
        assert_eq!(
            ToolIntentRequest::decode(intent.encode_to_vec().as_slice())
                .unwrap()
                .call_id,
            "toolu_1"
        );
        let exec = ExecuteHostToolRequest {
            call_id: "toolu_1".into(),
            ..Default::default()
        };
        assert_eq!(
            ExecuteHostToolRequest::decode(exec.encode_to_vec().as_slice())
                .unwrap()
                .call_id,
            "toolu_1"
        );
        let invoke = InvokeRequest {
            idempotency_key: "vamfx_x".into(),
            ..Default::default()
        };
        assert_eq!(
            InvokeRequest::decode(invoke.encode_to_vec().as_slice())
                .unwrap()
                .idempotency_key,
            "vamfx_x"
        );
        let status = RunStatus {
            resumes: 2,
            waiting_for: crate::v1::WaitingFor::Reauth as i32,
            ..Default::default()
        };
        let decoded = RunStatus::decode(status.encode_to_vec().as_slice()).unwrap();
        assert_eq!((decoded.resumes, decoded.waiting_for), (2, 4));
        let _ = EffectSpec::default();
    }

    #[test]
    fn run_request_permission_round_trips_in_json() {
        use crate::v1::CapabilityManifest;
        let json = r#"{"brokerId":"vam.sched","trustTier":"TRUST_TIER_FIRST_PARTY",
            "tools":[{"toolName":"arm"}],
            "runRequestPermission":{"grantTools":["arm"],"loopGuardedTools":["arm"],"surface":"schedule"}}"#;
        let manifest: CapabilityManifest =
            json_to_proto("vam.v1.CapabilityManifest", json).expect("decode");
        let permission = manifest.run_request_permission.clone().expect("present");
        assert_eq!(permission.surface, "schedule");
        let back = proto_to_json("vam.v1.CapabilityManifest", &manifest).expect("encode");
        let again: CapabilityManifest =
            json_to_proto("vam.v1.CapabilityManifest", &back).expect("redecode");
        assert_eq!(manifest, again);
    }

    #[test]
    fn notification_channel_round_trips_in_json() {
        use crate::v1::CapabilityManifest;
        let json = r#"{"brokerId":"vam.notify","trustTier":"TRUST_TIER_FIRST_PARTY",
            "notificationChannel":{"channel":"slack","surface":"slack","maxActionsPerMinute":30}}"#;
        let manifest: CapabilityManifest =
            json_to_proto("vam.v1.CapabilityManifest", json).expect("decode");
        let channel = manifest.notification_channel.clone().expect("present");
        assert_eq!(channel.surface, "slack");
        assert_eq!(channel.max_actions_per_minute, 30);
        let back = proto_to_json("vam.v1.CapabilityManifest", &manifest).expect("encode");
        assert!(back.contains("notificationChannel"));
        let again: CapabilityManifest =
            json_to_proto("vam.v1.CapabilityManifest", &back).expect("redecode");
        assert_eq!(manifest, again);
    }

    #[test]
    fn manifest_without_a_channel_encodes_unchanged() {
        use crate::v1::CapabilityManifest;
        use prost::Message;
        let plain = CapabilityManifest {
            broker_id: "b".into(),
            ..Default::default()
        };
        // An absent field 11 adds no bytes: the digest of an old manifest does not move.
        assert_eq!(
            plain.encode_to_vec(),
            CapabilityManifest {
                broker_id: "b".into(),
                notification_channel: None,
                ..Default::default()
            }
            .encode_to_vec()
        );
        assert_eq!(plain.encode_to_vec(), vec![0x0a, 0x01, b'b']);
    }

    #[test]
    fn schedule_broker_run_json_decodes_to_run_grant_request() {
        use crate::v1::{OnBusy, PlaceMode, RunGrantRequest};
        let json = r#"{"agentUrn":null,"workspace":{"mode":"SAME"},"thread":{"mode":"NEW","name":"daily"},
            "message":"summarise","playbookRef":null,"inputs":null,"onBusy":"QUEUE","expiresAt":null}"#;
        let request: RunGrantRequest =
            json_to_proto("vam.v1.RunGrantRequest", json).expect("short names decode");
        assert_eq!(
            request.workspace.as_ref().unwrap().mode,
            PlaceMode::Same as i32
        );
        assert_eq!(request.thread.as_ref().unwrap().mode, PlaceMode::New as i32);
        assert_eq!(request.thread.as_ref().unwrap().name, "daily");
        assert_eq!(request.on_busy, OnBusy::Queue as i32);

        let full = r#"{"workspace":{"mode":"PLACE_MODE_ID","id":"sp_x"},"onBusy":"ON_BUSY_SKIP","message":"m"}"#;
        let request: RunGrantRequest =
            json_to_proto("vam.v1.RunGrantRequest", full).expect("full names decode");
        assert_eq!(request.workspace.unwrap().mode, PlaceMode::Id as i32);
        assert_eq!(request.on_busy, OnBusy::Skip as i32);

        let unknown = r#"{"message":"m","surprise":1}"#;
        assert!(json_to_proto::<RunGrantRequest>("vam.v1.RunGrantRequest", unknown).is_err());
    }
}
