use std::fs;
use std::path::{Path, PathBuf};

use super::*;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e/world/fixtures")
}

fn squrell(rel: &str) -> PathBuf {
    fixtures()
        .join("marketplace-squrell/listings/microsoft-365")
        .join(rel)
}

/// Copy a directory tree (files and directories only).
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

fn skill_copy() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    copy_tree(&squrell("skills/m365-mail"), t.path());
    t
}

fn message(r: Result<CheckedPackage, PackageError>) -> String {
    r.expect_err("expected the check to fail").0
}

#[test]
fn the_squrell_fixture_artifacts_pass_each_check() {
    let skill = check_dir(&squrell("skills/m365-mail"), None).unwrap();
    assert_eq!(skill.kind, ArtifactKind::Skill);
    assert_eq!(
        skill.urn,
        "urn:vam:skill:hub.squrell.cloud/squrell/m365-mail"
    );
    assert_eq!(skill.version, "1.0.0");
    let agent = check_dir(&squrell("agents/executive-assistant"), None).unwrap();
    assert_eq!(agent.kind, ArtifactKind::Agent);
    let playbook = check_dir(
        &squrell("playbooks/morning-triage"),
        Some(ArtifactKind::Playbook),
    )
    .unwrap();
    assert_eq!(playbook.kind, ArtifactKind::Playbook);
    assert_eq!(
        playbook.urn,
        "urn:vam:playbook:hub.squrell.cloud/squrell/morning-triage"
    );
}

#[test]
fn a_skill_summary_names_tools_effects_and_reach() {
    let s = check_dir(&squrell("skills/m365-mail"), None)
        .unwrap()
        .summary;
    assert_eq!(
        s.requires_connector,
        "urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365"
    );
    assert!(!s.tools.is_empty());
    let t = s.tools.iter().find(|t| t.name == "mail_search").unwrap();
    assert_eq!(t.side_effect, "read_only");
    assert!(!t.approval_required);
    assert_eq!(t.egress[0].host, "graph.microsoft.com");
    assert_eq!(t.egress[0].paths[0].path, "/v1.0/me/messages");
    assert!(!t.egress[0].can_write);
    assert_eq!(s.egress_hosts(), vec!["graph.microsoft.com".to_string()]);
    // The listing writes (draft, send, create, and the POST that finds meeting times) but only to
    // exact paths, and nothing reaches any path.
    assert!(s.can_write());
    assert!(!s.writes_by_prefix());
    assert!(!s.reaches_any_path());
    for name in [
        "mail_draft",
        "mail_send",
        "calendar_create_event",
        "calendar_find_slots",
    ] {
        let t = s.tools.iter().find(|t| t.name == name).unwrap();
        assert!(t.egress[0].paths.iter().all(|p| p.exact), "{name}");
    }
    let send = s.tools.iter().find(|t| t.name == "mail_send").unwrap();
    assert!(send.approval_required);
    assert_eq!(send.side_effect, "mutable_external");
    assert_eq!(send.egress[0].paths[0].path, "/v1.0/me/sendMail");
    let json = serde_json::to_value(&s).unwrap();
    assert_eq!(json["kind"], "skill");
    assert_eq!(json["tools"][0]["sideEffect"], "read_only");
}

#[test]
fn a_slash_prefix_is_named_any_path() {
    let host = "notes.example.test";
    let reach = PathReach {
        path: "/".into(),
        exact: false,
        any_path: true,
    };
    assert_eq!(
        describe_reach(host, &reach),
        "any path on notes.example.test"
    );
    let community =
        fixtures().join("marketplace-community/listings/helpful-notes/skills/notes-search");
    let s = check_dir(&community, None).unwrap().summary;
    assert!(s.reaches_any_path());
    assert!(s.tools[0].egress[0]
        .text
        .contains("any path on notes.example.test"));
}

#[test]
fn an_agent_summary_lists_skills_and_rules() {
    let s = check_dir(&squrell("agents/executive-assistant"), None)
        .unwrap()
        .summary;
    assert_eq!(
        s.skills,
        vec!["urn:vam:skill:hub.squrell.cloud/squrell/m365-mail".to_string()]
    );
    assert_eq!(s.tool_default.as_deref(), Some("deny"));
    assert!(s
        .rules
        .iter()
        .any(|r| r.target == "skill:m365-mail/mail_search" && r.decision == "allow"));
    let rule = |target: &str| {
        s.rules
            .iter()
            .find(|r| r.target == target)
            .map(|r| r.decision.clone())
    };
    assert_eq!(rule("skill:m365-mail/mail_draft").as_deref(), Some("allow"));
    assert_eq!(rule("skill:m365-mail/mail_send").as_deref(), Some("ask"));
    assert_eq!(
        rule("skill:m365-mail/calendar_create_event").as_deref(),
        Some("ask")
    );
    assert_eq!(rule("builtin:schedule_create").as_deref(), Some("ask"));
    assert!(s.allow_any_playbook);
}

#[test]
fn kind_detection_needs_exactly_one_marker() {
    let t = tempfile::tempdir().unwrap();
    assert!(detect_kind(t.path())
        .unwrap_err()
        .0
        .contains("holds no artifact"));
    fs::write(t.path().join("agent.json"), "{}").unwrap();
    assert_eq!(detect_kind(t.path()).unwrap(), ArtifactKind::Agent);
    fs::write(t.path().join("playbook.json"), "{}").unwrap();
    assert!(detect_kind(t.path())
        .unwrap_err()
        .0
        .contains("more than one"));
    assert!(detect_kind(&t.path().join("nope"))
        .unwrap_err()
        .0
        .contains("not a directory"));
}

#[test]
fn a_skill_with_skill_md_passes_the_same_check() {
    let t = skill_copy();
    fs::rename(t.path().join("SKILLS.md"), t.path().join("SKILL.md")).unwrap();
    assert_eq!(detect_kind(t.path()).unwrap(), ArtifactKind::Skill);
    let skill = check_dir(t.path(), Some(ArtifactKind::Skill)).unwrap();
    assert_eq!(
        skill.urn,
        "urn:vam:skill:hub.squrell.cloud/squrell/m365-mail"
    );
    assert_eq!(skill_doc(t.path()), Some(t.path().join("SKILL.md")));
}

#[test]
fn skills_md_wins_when_both_exist() {
    let t = skill_copy();
    // A SKILL.md that would fail the check is ignored while SKILLS.md is there.
    fs::write(t.path().join("SKILL.md"), "no frontmatter").unwrap();
    assert_eq!(skill_doc(t.path()), Some(t.path().join("SKILLS.md")));
    check_dir(t.path(), None).unwrap();
    fs::remove_file(t.path().join("SKILLS.md")).unwrap();
    assert!(message(check_dir(t.path(), None)).contains("frontmatter"));
}

fn connector_dir(raw: &str) -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("connector.json"), raw).unwrap();
    t
}

fn community_connector() -> serde_json::Value {
    serde_json::json!({
        "urn": "urn:vam:connector:market.example.org/acme/bugwatch",
        "version": "1.0.0",
        "displayName": "Bugwatch",
        "publisher": "Acme Tools",
        "description": "Issues in Bugwatch.",
        "icon": "bugwatch",
        "provider": { "kind": "oauth2", "profile": "discover", "clientSecret": "none",
                      "scopes": ["read", "write"], "orgConsent": "none" },
        "connection": { "holder": "user", "multiAccount": false, "label": "whoami.email" },
        "mcp": { "url": "https://mcp.bugwatch.example.org/mcp", "tools": [
            { "remote": "list_issues", "sideEffect": "READ_ONLY" },
            { "remote": "save_issue", "sideEffect": "MUTABLE_EXTERNAL", "default": "ask" } ] }
    })
}

#[test]
fn a_connector_package_is_checked_and_summarised() {
    let t = connector_dir(&community_connector().to_string());
    assert_eq!(detect_kind(t.path()).unwrap(), ArtifactKind::Connector);
    let c = check_dir(t.path(), None).unwrap();
    assert_eq!(c.kind, ArtifactKind::Connector);
    assert_eq!(c.urn, "urn:vam:connector:market.example.org/acme/bugwatch");
    assert_eq!(c.version, "1.0.0");
    assert!(matches!(c.spec, CheckedSpec::Connector(_)));
    let s = c.summary.connector.as_ref().unwrap();
    assert_eq!(s.publisher, "Acme Tools");
    assert_eq!(s.profile, "discover");
    assert_eq!(s.mcp_host, "mcp.bugwatch.example.org");
    assert_eq!(s.hosts, ["mcp.bugwatch.example.org"]);
    assert_eq!(s.scopes, ["read", "write"]);
    assert_eq!(s.tools.len(), 2);
    assert_eq!(s.tools[1].side_effect, "mutable_external");
    assert_eq!(s.tools[1].default.as_deref(), Some("ask"));
    // The same kind expected by the caller.
    assert!(check_dir(t.path(), Some(ArtifactKind::Connector)).is_ok());
}

#[test]
fn a_connector_package_is_held_to_the_marketplace_limits() {
    let mut v = community_connector();
    v["urn"] = "urn:vam:connector:market.example.org/acme/linear".into();
    let m = message(check_dir(connector_dir(&v.to_string()).path(), None));
    assert!(
        m.starts_with("connector.json:") && m.contains("Squrell connector id"),
        "{m}"
    );
    let m = message(check_dir(connector_dir("{}").path(), None));
    assert!(m.starts_with("connector.json:"), "{m}");
}

#[test]
fn an_expected_kind_must_match_the_directory() {
    let m = message(check_dir(
        &squrell("skills/m365-mail"),
        Some(ArtifactKind::Agent),
    ));
    assert!(m.contains("agent.json"), "{m}");
}

#[test]
fn skill_rules_report_the_same_text_vamd_gives() {
    let t = skill_copy();
    fs::remove_file(t.path().join("manifest.json")).unwrap();
    assert_eq!(
        message(check_dir(t.path(), Some(ArtifactKind::Skill))),
        "manifest.json is required in the skill package"
    );

    let t = skill_copy();
    let md = fs::read_to_string(t.path().join("SKILLS.md"))
        .unwrap()
        .replace("squrell/m365-mail\"", "squrell/other\"");
    fs::write(t.path().join("SKILLS.md"), md).unwrap();
    assert!(message(check_dir(t.path(), None)).contains("must match manifest.json resource_id.urn"));

    let t = skill_copy();
    let md = fs::read_to_string(t.path().join("SKILLS.md"))
        .unwrap()
        .replace("1.0.0", "2.0.0");
    fs::write(t.path().join("SKILLS.md"), md).unwrap();
    assert!(message(check_dir(t.path(), None)).contains("frontmatter version '2.0.0' must match"));

    let t = skill_copy();
    fs::write(t.path().join("SKILLS.md"), "no frontmatter").unwrap();
    assert!(message(check_dir(t.path(), None)).contains("must begin with YAML frontmatter"));
}

#[test]
fn egress_rules_apply_offline() {
    let t = skill_copy();
    let raw = fs::read_to_string(t.path().join("manifest.json")).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v.as_object_mut().unwrap().remove("requiresConnector");
    fs::write(t.path().join("manifest.json"), v.to_string()).unwrap();
    assert!(message(check_dir(t.path(), None)).contains("requires_connector"));

    let t = skill_copy();
    let raw = fs::read_to_string(t.path().join("manifest.json")).unwrap();
    fs::write(
        t.path().join("manifest.json"),
        raw.replace("graph.microsoft.com", "GRAPH.example.com"),
    )
    .unwrap();
    assert!(message(check_dir(t.path(), None)).contains("egress"));
}

#[test]
fn a_bad_card_fails_offline_naming_tool_file_and_place() {
    let t = skill_copy();
    let raw = fs::read_to_string(t.path().join("manifest.json")).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["tools"][0]["ui"] = serde_json::json!({"show": "mail_search"});
    v["tools"][0]["outputSchemaJson"] = serde_json::json!(
        "{\"type\":\"object\",\"properties\":{\"messages\":{\"type\":\"array\"}}}"
    );
    fs::write(t.path().join("manifest.json"), v.to_string()).unwrap();
    let m = message(check_dir(t.path(), None));
    assert!(
        m.contains("mail_search") && m.contains("jigs/mail_search.json"),
        "{m}"
    );

    fs::create_dir_all(t.path().join("jigs")).unwrap();
    fs::write(
        t.path().join("jigs/mail_search.json"),
        r#"{"nodes":{"a":{"type":"Carousel"}},"root":"a"}"#,
    )
    .unwrap();
    let m = message(check_dir(t.path(), None));
    assert!(m.contains("jigs/mail_search.json"), "{m}");
}

#[test]
fn a_symlinked_manifest_is_not_read() {
    let t = skill_copy();
    let outside = tempfile::tempdir().unwrap();
    fs::copy(
        t.path().join("manifest.json"),
        outside.path().join("m.json"),
    )
    .unwrap();
    fs::remove_file(t.path().join("manifest.json")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("m.json"),
        t.path().join("manifest.json"),
    )
    .unwrap();
    let m = message(check_dir(t.path(), None));
    assert!(m.contains("manifest.json is required"), "{m}");
}

#[test]
fn agent_and_playbook_rules() {
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("agent.json"), "{}").unwrap();
    assert!(message(check_dir(t.path(), None)).contains("spec is required"));
    fs::write(t.path().join("agent.json"), "not json").unwrap();
    assert!(message(check_dir(t.path(), None)).starts_with("agent.json:"));
    let raw = fs::read_to_string(squrell("agents/executive-assistant/agent.json")).unwrap();
    fs::write(
        t.path().join("agent.json"),
        raw.replace(
            "urn:vam:agent:hub.squrell.cloud/squrell/executive-assistant",
            "urn:vam:playbook:hub.squrell.cloud/squrell/executive-assistant",
        ),
    )
    .unwrap();
    assert!(!message(check_dir(t.path(), None)).is_empty());

    let t = tempfile::tempdir().unwrap();
    let raw = fs::read_to_string(squrell("playbooks/morning-triage/playbook.json")).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["spec"]["activation"]["when"] = serde_json::json!("");
    fs::write(t.path().join("playbook.json"), v.to_string()).unwrap();
    assert!(message(check_dir(t.path(), None)).contains("activation.when"));
}

#[test]
fn publish_defaults_do_not_override_existing_permissions() {
    let mut a = AgentSpec::default();
    apply_publish_permission_defaults(&mut a);
    let tools = a.permissions.as_ref().unwrap().tools.as_ref().unwrap();
    assert_eq!(tools.default, ToolDefault::BySideEffect as i32);
    let before = a.clone();
    apply_publish_permission_defaults(&mut a);
    assert_eq!(a, before);
}

/// Rewrites the fixture's single egress shape to `methods` and the given prefix / exact paths.
fn skill_with_egress(
    methods: &[&str],
    prefixes: &[&str],
    paths: &[&str],
) -> (tempfile::TempDir, serde_json::Value) {
    let t = skill_copy();
    let raw = fs::read_to_string(t.path().join("manifest.json")).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let shape = &mut v["tools"][0]["egress"][0];
    shape["methods"] = serde_json::json!(methods);
    shape["pathPrefixes"] = serde_json::json!(prefixes);
    shape["paths"] = serde_json::json!(paths);
    v["tools"][0]["sideEffect"] = serde_json::json!("SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL");
    fs::write(t.path().join("manifest.json"), v.to_string()).unwrap();
    (t, v)
}

#[test]
fn an_exact_path_is_reported_as_exact_and_a_write_by_prefix_is_flagged() {
    let (t, _) = skill_with_egress(&["POST"], &[], &["/v1.0/me/events"]);
    let checked = check_dir(t.path(), None).unwrap();
    let e = &checked.summary.tools[0].egress[0];
    assert_eq!(
        e.paths,
        vec![PathReach {
            path: "/v1.0/me/events".into(),
            exact: true,
            any_path: false
        }]
    );
    assert!(e.can_write);
    assert!(!e.writes_by_prefix);
    assert!(e.text.contains("exactly /v1.0/me/events"), "{}", e.text);
    assert!(!checked.summary.writes_by_prefix());
    assert!(!checked.summary.reaches_any_path());
    // The listing's one standing warning: findMeetingTimes is a READ_ONLY POST.
    assert!(
        checked
            .warnings
            .iter()
            .all(|w| w.contains("calendar_find_slots")),
        "{:?}",
        checked.warnings
    );
    let json = serde_json::to_value(&checked.summary).unwrap();
    assert_eq!(json["tools"][0]["egress"][0]["paths"][0]["exact"], true);
    assert_eq!(json["tools"][0]["egress"][0]["writesByPrefix"], false);

    let (t, _) = skill_with_egress(&["POST"], &["/v1.0/me/events"], &[]);
    let checked = check_dir(t.path(), None).unwrap();
    let e = &checked.summary.tools[0].egress[0];
    assert!(e.writes_by_prefix && !e.paths[0].exact);
    assert!(checked.summary.writes_by_prefix());
    assert!(
        checked
            .warnings
            .iter()
            .any(|w| w.contains("by path prefix") && w.contains("mail_search")),
        "{:?}",
        checked.warnings
    );

    // A read by prefix is neither flagged nor warned about.
    let (t, _) = skill_with_egress(&["GET"], &["/v1.0/me/messages"], &["/v1.0/me/mailFolders"]);
    let checked = check_dir(t.path(), None).unwrap();
    assert!(!checked.summary.writes_by_prefix());
    assert_eq!(checked.summary.tools[0].egress[0].paths.len(), 2);
}

#[test]
fn a_malformed_exact_path_fails_offline() {
    for bad in [
        "/v1.0/me/events/",
        "v1.0/me/events",
        "/a/../b",
        "/a%2Fb",
        "/",
    ] {
        let (t, _) = skill_with_egress(&["POST"], &[], &[bad]);
        let m = message(check_dir(t.path(), None));
        assert!(m.contains("exact path"), "{bad}: {m}");
    }
    let (t, _) = skill_with_egress(&["POST"], &[], &[]);
    assert!(message(check_dir(t.path(), None)).contains("path_prefixes"));
}
