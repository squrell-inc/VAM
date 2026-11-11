use std::fs;
use std::path::{Path, PathBuf};

use super::*;

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
