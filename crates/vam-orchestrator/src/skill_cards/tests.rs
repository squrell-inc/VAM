use super::*;
use std::fs;

pub(crate) const MAIL_LIST: &str = r#"{"version":1,"catalog":"vam/1","root":"root","state":{},"nodes":{
 "root":{"type":"List","props":{"items":{"$data":"/messages"}},"children":["t"]},
 "t":{"type":"Text","props":{"text":{"$item":"/subject"}}}}}"#;
const OUT: &str = r#"{"type":"object","required":["messages"],"properties":{"messages":{"type":"array","items":{"type":"object","properties":{"from":{"type":"string"},"subject":{"type":"string"}}}}}}"#;

pub(crate) fn manifest(show: &str, output_schema: &str) -> SkillManifest {
    let json = serde_json::json!({
        "resourceId": {"urn": "urn:vam:skill:local.vam.dev/e2e/mail"},
        "version": {"major": 1, "minor": 0, "patch": 0},
        "description": "t",
        "tools": [{
            "name": "mail_list", "description": "d",
            "inputSchemaJson": "{\"type\":\"object\"}",
            "outputSchemaJson": output_schema,
            "ui": {"show": show},
        }]
    });
    vam_spec::encoding::json_to_proto("vam.v1.SkillManifest", &json.to_string()).expect("manifest")
}

pub(crate) fn skill_dir(card: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let skill = dir.path().join("mail");
    fs::create_dir_all(skill.join("jigs")).unwrap();
    fs::write(
        skill.join("manifest.json"),
        serde_json::to_string(&serde_json::json!({
            "resourceId": {"urn": "urn:vam:skill:local.vam.dev/e2e/mail"},
            "version": {"major": 1, "minor": 0, "patch": 0},
            "description": "t",
            "tools": [{"name": "mail_list", "description": "d",
              "inputSchemaJson": "{\"type\":\"object\"}", "outputSchemaJson": OUT,
              "ui": {"show": "mail_list"}}]
        }))
        .unwrap(),
    )
    .unwrap();
    if let Some(card) = card {
        fs::write(skill.join("jigs/mail_list.json"), card).unwrap();
    }
    dir
}

#[test]
fn a_fitting_card_is_kept_with_its_name_and_schema() {
    let dir = skill_dir(Some(MAIL_LIST));
    let cards = check_package_cards(&dir.path().join("mail"), &manifest("mail_list", OUT)).unwrap();
    let card = cards["mail_list"].show.clone().unwrap();
    assert_eq!(card.jig, "mail.mail_list");
    assert_eq!(card.ir["jig"], "mail.mail_list");
    assert!(card.ir["inputSchema"].is_object());
}

#[test]
fn a_missing_card_file_names_tool_and_file() {
    let dir = skill_dir(None);
    let err =
        check_package_cards(&dir.path().join("mail"), &manifest("mail_list", OUT)).unwrap_err();
    assert!(
        err.contains("mail_list") && err.contains("jigs/mail_list.json"),
        "{err}"
    );
}

#[test]
fn an_unknown_primitive_names_it_and_its_pointer() {
    let bad = MAIL_LIST.replace("\"Text\"", "\"Carousel\"");
    let dir = skill_dir(Some(&bad));
    let err =
        check_package_cards(&dir.path().join("mail"), &manifest("mail_list", OUT)).unwrap_err();
    assert!(
        err.contains("Carousel") && err.contains("/nodes/t"),
        "{err}"
    );
}

#[test]
fn a_binding_the_output_schema_lacks_names_the_path() {
    let bad = MAIL_LIST.replace("/subject", "/sender");
    let dir = skill_dir(Some(&bad));
    let err =
        check_package_cards(&dir.path().join("mail"), &manifest("mail_list", OUT)).unwrap_err();
    assert!(
        err.contains("/sender") && err.contains("/messages"),
        "{err}"
    );
}

#[test]
fn a_card_that_is_not_json_or_too_big_is_refused() {
    let dir = skill_dir(Some("[1]"));
    assert!(check_package_cards(&dir.path().join("mail"), &manifest("mail_list", OUT)).is_err());
    let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(70_000));
    let dir = skill_dir(Some(&big));
    let err =
        check_package_cards(&dir.path().join("mail"), &manifest("mail_list", OUT)).unwrap_err();
    assert!(err.contains("larger than"), "{err}");
}

#[test]
fn a_skill_without_ui_has_no_cards() {
    let mut m = manifest("mail_list", OUT);
    m.tools[0].ui = None;
    let dir = skill_dir(None);
    assert!(check_package_cards(&dir.path().join("mail"), &m)
        .unwrap()
        .is_empty());
}

#[test]
fn seeding_keeps_the_checked_card_and_a_bad_one_is_declared_but_absent() {
    let dir = skill_dir(Some(MAIL_LIST));
    seed_session("sc-ok", dir.path());
    assert!(cards_for("sc-ok", "mail_list").unwrap().show.is_some());
    let dir = skill_dir(Some("{}"));
    seed_session("sc-bad", dir.path());
    let c = cards_for("sc-bad", "mail_list").unwrap();
    assert!(c.declares_show && c.show.is_none());
    assert!(cards_for("sc-ok", "other").is_none());
}

#[test]
fn forgetting_a_session_drops_its_cards() {
    let dir = skill_dir(Some(MAIL_LIST));
    seed_session("sc-forget", dir.path());
    assert!(cards_for("sc-forget", "mail_list").is_some());
    forget_session("sc-forget");
    assert!(cards_for("sc-forget", "mail_list").is_none());
}
