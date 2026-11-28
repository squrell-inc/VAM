use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::package::check_dir;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e/world/fixtures")
}

fn index(name: &str) -> String {
    fs::read_to_string(fixtures().join(name).join("marketplace.json")).unwrap()
}

fn good() -> serde_json::Value {
    serde_json::from_str(&index("marketplace-squrell")).unwrap()
}

fn problems(v: serde_json::Value) -> Vec<String> {
    match parse_index(&v.to_string()) {
        Ok(_) => vec![],
        Err(p) => p.iter().map(|p| p.to_string()).collect(),
    }
}

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

#[test]
fn the_fixture_indexes_parse() {
    let squrell = parse_index(&index("marketplace-squrell")).unwrap();
    assert_eq!(squrell.publisher_domain, "hub.squrell.cloud");
    assert_eq!(squrell.listings[0].git_ref, "microsoft-365-v1.0.0");
    assert_eq!(squrell.listings[0].artifacts.len(), 3);
    let community = parse_index(&index("marketplace-community")).unwrap();
    assert_eq!(community.listings.len(), 4);
    assert!(parse_index(&index("marketplace-community-broken")).is_err());
}

#[test]
fn index_rules_name_the_listing() {
    assert!(problems(good()).is_empty());

    let mut v = good();
    v["publisherDomain"] = "Not A Domain".into();
    assert!(problems(v)[0].contains("publisherDomain"));

    let mut v = good();
    v["listings"][0]["id"] = "Bad Id".into();
    assert!(problems(v)[0].starts_with("listing 'Bad Id': id"));

    let mut v = good();
    let dup = v["listings"][0].clone();
    v["listings"].as_array_mut().unwrap().push(dup);
    assert!(problems(v).iter().any(|p| p.contains("appears twice")));

    let mut v = good();
    v["listings"][0]["artifacts"] = serde_json::json!([]);
    assert!(problems(v)[0].contains("at least one artifact"));

    let mut v = good();
    v["listings"][0]["connector"] = "microsoft-365".into();
    assert!(problems(v)[0].contains("connector"));

    let mut v = good();
    v["listings"][0]["extra"] = 1.into();
    assert!(problems(v)[0].contains("not a valid index"));
}

#[test]
fn a_connector_artifact_kind_is_part_of_the_index() {
    let mut v = good();
    v["listings"][0]["artifacts"][0]["kind"] = "connector".into();
    assert!(parse_index(&v.to_string()).is_ok());
    let mut v = good();
    v["listings"][0]["artifacts"][0]["kind"] = "widget".into();
    assert!(problems(v)[0].contains("not a valid index"));
}

#[test]
fn artifact_paths_stay_inside_the_repository() {
    for bad in ["", "/etc", "../x", "a/../b", "a//b", "./a", "a\\b", "a/"] {
        assert!(check_relative_path(bad).is_err(), "{bad}");
    }
    assert!(check_relative_path("listings/a/skills/b").is_ok());
    let mut v = good();
    v["listings"][0]["artifacts"][0]["path"] = "../escape".into();
    assert!(problems(v)[0].contains("artifact path '../escape'"));
}

#[test]
fn refs_are_tag_names_that_are_safe_for_git() {
    for ok in ["v1.0.0", "microsoft-365-v1.0.0", "rel/1.2"] {
        assert!(check_git_ref(ok).is_ok(), "{ok}");
    }
    for bad in [
        "",
        "-rf",
        "--upload-pack=x",
        "a b",
        "a..b",
        "a/",
        "/a",
        ".a",
        "a.lock",
        "a~1",
        "a^",
        "a:b",
        "a?b",
        "a*b",
        "a[b",
        "a\\b",
        "a@{1}",
        "0123456789abcdef0123456789abcdef01234567",
    ] {
        assert!(check_git_ref(bad).is_err(), "{bad}");
    }
}

#[test]
fn node_requirements_compare_versions() {
    let r = NodeRequirement::parse(">=0.1.0").unwrap();
    assert!(r.matches("0.1.0") && r.matches("0.2.3") && r.matches("1.0.0-rc.1+abc"));
    assert!(!r.matches("0.0.9") && !r.matches("garbage"));
    let r = NodeRequirement::parse(">=0.2, <1").unwrap();
    assert!(r.matches("0.9.9") && !r.matches("1.0.0") && !r.matches("0.1.9"));
    assert!(NodeRequirement::parse("=1.2.3").unwrap().matches("1.2.3"));
    assert!(!NodeRequirement::parse(">1.2.3").unwrap().matches("1.2.3"));
    for bad in [
        "",
        "0.1.0",
        "~0.1",
        ">=",
        ">=a.b",
        ">=1.2.3.4",
        ">=1,",
        ">= -1",
    ] {
        assert!(NodeRequirement::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn urn_domain_reads_the_registry() {
    assert_eq!(
        urn_domain("urn:vam:skill:hub.squrell.cloud/squrell/m"),
        Some("hub.squrell.cloud")
    );
    assert_eq!(urn_domain("urn:vam:skill:nodomain"), None);
    assert_eq!(urn_domain("junk"), None);
}

#[test]
fn the_squrell_fixture_validates_clean() {
    let r = check_marketplace_root(&fixtures().join("marketplace-squrell"));
    assert_eq!(r.errors(), 0, "{:?}", r.findings);
    assert!(
        r.findings
            .iter()
            .all(|f| !f.message.contains("publisherDomain")),
        "{:?}",
        r.findings
    );
    assert_eq!(r.artifacts.len(), 3);
    assert!(r
        .artifacts
        .iter()
        .any(|a| a.kind == ArtifactKind::Skill && !a.summary.tools.is_empty()));
}

#[test]
fn the_community_fixture_has_no_errors_and_warns_on_the_clashing_domains() {
    let r = check_marketplace_root(&fixtures().join("marketplace-community"));
    assert_eq!(r.errors(), 0, "{:?}", r.findings);
    let warned: Vec<&str> = r
        .findings
        .iter()
        .filter(|f| f.level == Level::Warning && f.message.contains("publisherDomain"))
        .filter_map(|f| f.listing.as_deref())
        .collect();
    assert!(warned.contains(&"claims-the-squrell-name"), "{warned:?}");
    assert!(warned.contains(&"claims-the-org-name"), "{warned:?}");
    assert!(!warned.contains(&"helpful-notes"));
}

#[test]
fn a_broken_index_is_one_error() {
    let r = check_marketplace_root(&fixtures().join("marketplace-community-broken"));
    assert_eq!(r.errors(), 1);
    assert!(r.findings[0].message.contains("not a valid index"));
}

#[test]
fn one_bad_artifact_does_not_hide_another_listing() {
    let t = tempfile::tempdir().unwrap();
    let mut v = good();
    let mut second = v["listings"][0].clone();
    second["id"] = "second".into();
    second["artifacts"] = serde_json::json!([{"kind": "agent", "path": "missing/dir"}]);
    v["listings"].as_array_mut().unwrap().push(second);
    fs::write(t.path().join("marketplace.json"), v.to_string()).unwrap();
    copy_tree(
        &fixtures().join("marketplace-squrell/listings"),
        &t.path().join("listings"),
    );
    let r = check_marketplace_root(t.path());
    assert_eq!(r.errors(), 1, "{:?}", r.findings);
    let error = r.findings.iter().find(|f| f.level == Level::Error).unwrap();
    assert_eq!(error.listing.as_deref(), Some("second"));
    assert_eq!(r.artifacts.len(), 3);
}

#[test]
fn a_symlinked_index_or_artifact_directory_is_refused() {
    let outside = tempfile::tempdir().unwrap();
    fs::write(
        outside.path().join("marketplace.json"),
        index("marketplace-squrell"),
    )
    .unwrap();
    let t = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("marketplace.json"),
        t.path().join("marketplace.json"),
    )
    .unwrap();
    let r = check_marketplace_root(t.path());
    assert_eq!(r.errors(), 1);
    assert!(
        r.findings[0].message.contains("regular file"),
        "{:?}",
        r.findings
    );

    let t = tempfile::tempdir().unwrap();
    fs::write(
        t.path().join("marketplace.json"),
        index("marketplace-squrell"),
    )
    .unwrap();
    fs::create_dir_all(t.path().join("listings/microsoft-365")).unwrap();
    std::os::unix::fs::symlink(
        outside.path(),
        t.path().join("listings/microsoft-365/skills"),
    )
    .unwrap();
    let r = check_marketplace_root(t.path());
    assert!(
        r.findings.iter().any(|f| f.message.contains("symlink")),
        "{:?}",
        r.findings
    );
}

#[test]
fn node_requirement_accepts_dev_and_rc_builds_of_the_required_version() {
    let r = NodeRequirement::parse(">=0.1.0").unwrap();
    for v in ["0.1.0+dev", "0.1.0+dev.14", "0.1.0-rc.1"] {
        assert!(r.matches(v), "{v}");
    }
    assert!(!r.matches("0.0.9+dev.3"));
}

// ---- the listing check (CN-7.3) ------------------------------------------------------------------

fn packages(dirs: &[&str]) -> Vec<CheckedPackage> {
    dirs.iter()
        .map(|d| check_dir(&fixtures().join(d), None).unwrap())
        .collect()
}

const KIT_TOOL: &str = "connector-kit/broken-tool/listings/notesdesk";
const KIT_FILE: &str = "connector-kit/broken-file/listings/kit-broken";

fn kit_tool_listing() -> Vec<CheckedPackage> {
    packages(&[
        &format!("{KIT_TOOL}/connector"),
        &format!("{KIT_TOOL}/skills/notesdesk-notes"),
    ])
}

fn errors_of(findings: &[Finding]) -> Vec<&str> {
    findings
        .iter()
        .filter(|f| f.level == Level::Error)
        .map(|f| f.message.as_str())
        .collect()
}

fn warnings_of(findings: &[Finding]) -> Vec<&str> {
    findings
        .iter()
        .filter(|f| f.level == Level::Warning)
        .map(|f| f.message.as_str())
        .collect()
}

#[test]
fn a_listing_whose_tools_stay_in_their_connector_has_no_finding() {
    assert!(check_listing("notesdesk", &kit_tool_listing()).is_empty());
    let squrell = packages(&["marketplace-squrell/listings/microsoft-365/skills/m365-mail"]);
    // The connector is built in: its binding `graph` and `graph.microsoft.com` are checked.
    assert!(check_listing("microsoft-365", &squrell).is_empty());
}

const TICKETDESK: &str = "marketplace-community-connectors/listings/ticketdesk-connector";

fn ticketdesk_listing() -> Vec<CheckedPackage> {
    packages(&[
        &format!("{TICKETDESK}/connectors/ticketdesk"),
        &format!("{TICKETDESK}/skills/ticketdesk-tickets"),
    ])
}

#[test]
fn a_skill_host_outside_its_connectors_company_address_is_refused() {
    assert!(check_listing("ticketdesk-connector", &ticketdesk_listing()).is_empty());
    let mut p = ticketdesk_listing();
    p[1].summary.tools[0].egress[0].host = "{subdomain}.elsewhere.example".into();
    let f = check_listing("ticketdesk-connector", &p);
    let e = errors_of(&f);
    // The templated rule says it; the binding rule does not say it twice.
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].contains(
        "which is not the company address (suffix `ticketdesk.example`) of the connector it requires"
    ));
}

#[test]
fn connectors_with_overlapping_placeholders_are_refused() {
    let p = packages(&[
        &format!("{KIT_FILE}/connectors/desk"),
        &format!("{KIT_FILE}/connectors/desk-api"),
    ]);
    let f = check_listing("kit-broken", &p);
    let e = errors_of(&f);
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(
        e[0].starts_with("listing `kit-broken`: the connectors "),
        "{e:?}"
    );
    assert!(e[0].contains("cannot tell apart") && e[0].contains("vamsec_desk_api"));
    assert!(f.iter().all(|f| f.listing.as_deref() == Some("kit-broken")));
}

#[test]
fn a_urn_named_twice_is_refused() {
    let mut p = kit_tool_listing();
    p.push(p[1].clone());
    let f = check_listing("notesdesk", &p);
    assert_eq!(errors_of(&f).len(), 1, "{f:?}");
    assert!(errors_of(&f)[0]
        .ends_with("names urn:vam:skill:kit.community.example/community/notesdesk-notes twice"));
}

#[test]
fn two_connectors_with_one_id_are_refused() {
    let mut p = packages(&[&format!("{KIT_TOOL}/connector")]);
    let mut other = p[0].clone();
    other.urn = "urn:vam:connector:kit.community.example/elsewhere/notesdesk".into();
    if let CheckedSpec::Connector(def) = &mut other.spec {
        def.urn = other.urn.clone();
        def.bindings.as_mut().unwrap()[0].placeholder = "vamsec_elsewhere_other".into();
    }
    p.push(other);
    let f = check_listing("notesdesk", &p);
    let e = errors_of(&f);
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].contains("have the same id `notesdesk`: an id names one connector"));
}

#[test]
fn a_tool_must_name_a_binding_of_its_connector() {
    let p = packages(&[
        &format!("{KIT_FILE}/connectors/desk"),
        &format!("{KIT_FILE}/skills/desk-files"),
    ]);
    let f = check_listing("kit-broken", &p);
    let e = errors_of(&f);
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].contains(
        "tool `file_read` names the binding `files` for the host `api.elsewhere.example`"
    ));
    assert!(
        e[0].contains("does not declare (its bindings: `api_x`)"),
        "{e:?}"
    );
}

#[test]
fn a_tool_must_name_one_of_its_bindings_hosts() {
    let mut p = kit_tool_listing();
    if let Some(t) = p[1].summary.tools.get_mut(0) {
        t.egress[0].host = "api.elsewhere.example".into();
    }
    let f = check_listing("notesdesk", &p);
    let e = errors_of(&f);
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].contains(
        "tool `note_search` names the host `api.elsewhere.example` on the binding `api`"
    ));
    assert!(
        e[0].contains("(its hosts: `api.notesdesk.example`)"),
        "{e:?}"
    );
}

#[test]
fn a_templated_host_must_be_one_the_connectors_binding_lists() {
    let mut p = ticketdesk_listing();
    if let CheckedSpec::Connector(def) = &mut p[0].spec {
        def.bindings.as_mut().unwrap()[0].hosts = vec!["{subdomain}.ticketdesk.example".into()];
        let name = def.bindings.as_ref().unwrap()[0].name.clone();
        p[1].summary.tools[0].egress[0].binding = name;
    }
    assert!(check_listing("ticketdesk-connector", &p).is_empty());
    let host = p[1].summary.tools[0].egress[0].host.clone();
    assert!(host.starts_with("{subdomain}."), "{host}");
    // The binding lists another company address host: the tool's is not one of them.
    if let CheckedSpec::Connector(def) = &mut p[0].spec {
        def.bindings.as_mut().unwrap()[0].hosts = vec!["{subdomain}.api.ticketdesk.example".into()];
    }
    let e = errors_of(&check_listing("ticketdesk-connector", &p))
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    // One per tool that names it.
    assert_eq!(e.len(), 2, "{e:?}");
    assert!(e.iter().all(|m| m.contains("does not list")), "{e:?}");
}

#[test]
fn a_connector_that_is_not_in_the_listing_or_built_in_is_a_warning_only() {
    let mut p = kit_tool_listing();
    p.remove(0);
    let f = check_listing("notesdesk", &p);
    assert!(errors_of(&f).is_empty(), "{f:?}");
    let w = warnings_of(&f);
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].contains("which this listing does not ship and no node builds in"));
}

#[test]
fn a_listing_with_a_connector_and_no_test_file_warns() {
    let t = tempfile::tempdir().unwrap();
    copy_tree(&fixtures().join("connector-kit/broken-tool"), t.path());
    let r = check_marketplace_root(t.path());
    assert!(!warnings_of(&r.findings)
        .iter()
        .any(|w| w.contains("connector-test.json")));
    fs::remove_file(
        t.path()
            .join("listings/notesdesk/tests/connector-test.json"),
    )
    .unwrap();
    let r = check_marketplace_root(t.path());
    assert_eq!(r.errors(), 0, "{:?}", r.findings);
    assert!(
        warnings_of(&r.findings).contains(
            &"the listing ships a connector but no listings/notesdesk/tests/connector-test.json: `vam connector test` has nothing to run"
        ),
        "{:?}",
        r.findings
    );
}

/// What vamd's import refuses for the content of a listing (the scenarios' `contains` words), and
/// the offline check says the same: one listing per fixture marketplace listing the scenarios have
/// installs refuse. Node policy (a marketplace's domain, trust) is not offline and is not here.
#[test]
fn every_refused_fixture_listing_is_refused_offline_with_the_imports_words() {
    const REFUSED: [(&str, &str, &str); 8] = [
        (
            "marketplace-community-connectors",
            "reserved-connector-id",
            "linear",
        ),
        (
            "marketplace-community-connectors",
            "unknown-profile-connector",
            "profile",
        ),
        (
            "marketplace-community-connectors",
            "connector-with-a-secret",
            "secret",
        ),
        (
            "marketplace-community-connectors",
            "org-secret-without-org-client",
            "client",
        ),
        (
            "marketplace-community-connectors",
            "api-key-with-two-fields",
            "one field",
        ),
        (
            "marketplace-community-connectors",
            "company-address-outside-its-suffix",
            "suffix",
        ),
        (
            "marketplace-community-connectors",
            "rest-connector-with-a-foreign-placeholder",
            "placeholder",
        ),
        (
            "connector-kit/broken-file",
            "kit-broken",
            "cannot tell apart",
        ),
    ];
    for (fixture, listing, words) in REFUSED {
        let r = check_marketplace_root(&fixtures().join(fixture));
        assert!(
            r.findings.iter().any(|f| f.level == Level::Error
                && f.listing.as_deref() == Some(listing)
                && f.message.contains(words)),
            "{listing} is not refused with `{words}`: {:?}",
            r.findings
        );
    }
    // No other listing of the fixtures is refused offline.
    for fixture in [
        "marketplace-squrell",
        "marketplace-community",
        "marketplace-community-connectors",
        "connector-kit/broken-file",
        "connector-kit/broken-tool",
    ] {
        let r = check_marketplace_root(&fixtures().join(fixture));
        for f in r.findings.iter().filter(|f| f.level == Level::Error) {
            let id = f.listing.as_deref().unwrap_or_default();
            assert!(
                REFUSED.iter().any(|(fx, l, _)| *fx == fixture && *l == id),
                "{fixture}: {f:?}"
            );
        }
    }
}

fn with_access(access: serde_json::Value) -> serde_json::Value {
    let mut v = good();
    v["listings"][0]["access"] = access;
    v
}

#[test]
fn access_is_optional_and_absent_serializes_nothing() {
    let idx = parse_index(&index("marketplace-squrell")).unwrap();
    assert!(idx.listings[0].access.is_none());
    let text = serde_json::to_string(&idx).unwrap();
    assert!(!text.contains("access"));
}

#[test]
fn every_valid_access_pair_parses() {
    for (vis, mode) in [
        ("org", "everyone"),
        ("org", "open"),
        ("org", "approval"),
        ("restricted", "open"),
        ("restricted", "approval"),
    ] {
        let v = with_access(serde_json::json!({"default": {"visibility": vis, "mode": mode}}));
        let idx = parse_index(&v.to_string()).unwrap_or_else(|p| panic!("{vis}/{mode}: {p:?}"));
        let d = idx.listings[0].access.as_ref().unwrap().default;
        let back = serde_json::to_value(d).unwrap();
        assert_eq!(back, serde_json::json!({"visibility": vis, "mode": mode}));
    }
}

#[test]
fn a_restricted_agent_open_to_everyone_is_refused() {
    let v = with_access(
        serde_json::json!({"default": {"visibility": "restricted", "mode": "everyone"}}),
    );
    let p = problems(v);
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(
        p[0].starts_with("listing 'microsoft-365': access.default"),
        "{p:?}"
    );
    assert!(p[0]
        .contains("a restricted agent cannot be open to everyone: use mode 'open' or 'approval'"));
}

#[test]
fn unknown_access_values_and_keys_are_refused() {
    let bad = [
        serde_json::json!({"default": {"visibility": "public", "mode": "open"}}),
        serde_json::json!({"default": {"visibility": "org", "mode": "Open"}}),
        serde_json::json!({"default": {"visibility": "org"}}),
        serde_json::json!({"default": {"visibility": "org", "mode": "open", "extra": 1}}),
        serde_json::json!({"default": {"visibility": "org", "mode": "open"}, "admins": ["u"]}),
        serde_json::json!({"admins": ["u"]}),
    ];
    for a in bad {
        let v = with_access(a.clone());
        assert!(parse_index(&v.to_string()).is_err(), "{a}");
    }
}
