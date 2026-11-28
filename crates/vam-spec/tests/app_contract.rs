//! AS-0.1 contract checks: the `App` message and the `app_id` fields keep their
//! field numbers, and the JSON shape is the one clients read and send.

use prost_reflect::DescriptorPool;
use vam_spec::encoding::{json_to_proto, proto_to_json, DESCRIPTOR_POOL};
use vam_spec::v1::*;

fn pool() -> &'static DescriptorPool {
    &DESCRIPTOR_POOL
}

fn number(message: &str, field: &str) -> u32 {
    pool()
        .get_message_by_name(message)
        .unwrap_or_else(|| panic!("missing message {message}"))
        .get_field_by_name(field)
        .unwrap_or_else(|| panic!("missing field {message}.{field}"))
        .number()
}

#[test]
fn app_id_field_numbers_are_locked() {
    assert_eq!(number("vam.v1.Space", "app_id"), 8);
    assert_eq!(number("vam.v1.RunSpec", "app_id"), 14);
    assert_eq!(number("vam.v1.RunStatus", "app_id"), 24);
    assert_eq!(number("vam.v1.CallerContext", "app_id"), 11);
}

#[test]
fn app_field_numbers_are_locked() {
    let fields = [
        "app_id",
        "name",
        "kind",
        "agent_urn",
        "agent_version",
        "owner_user_id",
        "created_at",
        "deactivated_at",
    ];
    for (i, f) in fields.iter().enumerate() {
        assert_eq!(number("vam.v1.App", f), i as u32 + 1, "App.{f}");
    }
    let count = pool()
        .get_message_by_name("vam.v1.App")
        .unwrap()
        .fields()
        .count();
    assert_eq!(count, 8);
}

#[test]
fn app_json_round_trips_with_kind_shared() {
    let json = r#"{"appId":"app_abc","name":"Team","kind":"shared","agentUrn":"urn:vam:agent:x","agentVersion":"1.0.0","ownerUserId":"alice"}"#;
    let app: App = json_to_proto("vam.v1.App", json).unwrap();
    assert_eq!(app.app_id, "app_abc");
    assert_eq!(app.kind, "shared");
    assert!(app.deactivated_at.is_none());
    let out = proto_to_json("vam.v1.App", &app).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["kind"], "shared");
    assert_eq!(v["appId"], "app_abc");
    assert!(v.get("deactivatedAt").is_none());
    let back: App = json_to_proto("vam.v1.App", &out).unwrap();
    assert_eq!(back, app);
}

#[test]
fn empty_app_id_prints_no_app_id() {
    let space = Space {
        space_id: "sp_1".into(),
        ..Default::default()
    };
    let v: serde_json::Value =
        serde_json::from_str(&proto_to_json("vam.v1.Space", &space).unwrap()).unwrap();
    assert!(v.get("appId").is_none());

    let status = RunStatus::default();
    let v: serde_json::Value =
        serde_json::from_str(&proto_to_json("vam.v1.RunStatus", &status).unwrap()).unwrap();
    assert!(v.get("appId").is_none());
}

#[test]
fn run_spec_json_with_app_id_parses() {
    let req: CreateRunRequest =
        json_to_proto("vam.v1.CreateRunRequest", r#"{"spec":{"appId":"app_x"}}"#).unwrap();
    assert_eq!(req.spec.unwrap().app_id, "app_x");
}
