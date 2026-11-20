//! `vam connector init|validate|test` through the built binary: the output, the exit codes and the
//! alias. No node, no host, no key.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn vam(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vam"))
        .args(args)
        .env_clear()
        .env("HOME", std::env::temp_dir())
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/world/fixtures")
        .join(name)
}

fn init_args<'a>(dir: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec![
        "connector",
        "init",
        dir,
        "--id",
        "notesdesk",
        "--auth",
        "api_key",
        "--host",
        "api.notesdesk.example",
        "--domain",
        "kit.community.example",
    ];
    v.extend_from_slice(extra);
    v
}

#[test]
fn init_then_validate_golden_output() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("kit");
    let d = dir.to_str().unwrap();
    let out = vam(&init_args(d, &["--runtime", "python3"]));
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        text(&out),
        format!(
            "wrote marketplace.json
wrote listings/notesdesk/connector/connector.json
wrote listings/notesdesk/skills/notesdesk-read/manifest.json
wrote listings/notesdesk/skills/notesdesk-read/SKILLS.md
wrote listings/notesdesk/skills/notesdesk-read/read.py
wrote listings/notesdesk/agents/notesdesk-assistant/agent.json
wrote listings/notesdesk/tests/connector-test.json
next: vam connector validate {d}   (then `vam connector test {d}` once it is available)
"
        )
    );

    for dir_arg in [d.to_string(), format!("{d}/listings/notesdesk")] {
        let out = vam(&["connector", "validate", &dir_arg, "--strict"]);
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(
            text(&out),
            "ok connector urn:vam:connector:kit.community.example/community/notesdesk
ok skill urn:vam:skill:kit.community.example/community/notesdesk-read
ok agent urn:vam:agent:kit.community.example/community/notesdesk-assistant
3 artifact(s) ok, 0 error(s), 0 warning(s)
"
        );
    }
}

#[test]
fn validate_names_what_the_node_would_refuse_and_exits_6() {
    let out = vam(&[
        "connector",
        "validate",
        fixture("connector-kit/broken-file").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(6), "{}", text(&out));
    let t = text(&out);
    for words in [
        "cannot tell apart",
        "binding `files`",
        "api.elsewhere.example",
    ] {
        assert!(t.contains(words), "{words}: {t}");
    }
    assert!(
        t.contains("error   kit-broken: listing `kit-broken`: the connectors "),
        "{t}"
    );
    assert!(t.contains("ok skill urn:vam:skill:kitbroken.community.example/community/desk-files"));
    assert!(
        t.contains("3 artifact(s) ok, 2 error(s), 0 warning(s)"),
        "{t}"
    );
    assert!(
        t.trim_end()
            .ends_with("Error: validation failed: 2 error(s)"),
        "{t}"
    );
}

#[test]
fn a_warning_fails_only_with_strict_and_json_lists_the_findings() {
    let squrell = fixture("marketplace-squrell");
    let squrell = squrell.to_str().unwrap();
    let out = vam(&["connector", "validate", squrell]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("warning microsoft-365 listings/microsoft-365/skills/m365-mail:"));
    let out = vam(&["connector", "validate", squrell, "--strict"]);
    assert_eq!(out.status.code(), Some(6), "{}", text(&out));
    let out = vam(&["--json", "connector", "validate", squrell]);
    assert!(out.status.success(), "{}", text(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["errors"], 0);
    assert_eq!(v["warnings"], 1);
    assert_eq!(v["artifacts"].as_array().unwrap().len(), 3);
}

#[test]
fn init_refuses_what_validate_would_and_what_it_cannot_overwrite() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("kit");
    let d = dir.to_str().unwrap();
    let out = vam(&[
        "connector",
        "init",
        d,
        "--id",
        "linear",
        "--auth",
        "api_key",
        "--host",
        "api.linear.example",
        "--domain",
        "kit.community.example",
    ]);
    assert_eq!(out.status.code(), Some(6), "{}", text(&out));
    assert!(
        text(&out).contains("is a Squrell connector id"),
        "{}",
        text(&out)
    );
    assert!(!dir.exists());

    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join(".keep"), "").unwrap();
    assert!(vam(&init_args(d, &[])).status.success());
    let out = vam(&init_args(d, &[]));
    assert_eq!(out.status.code(), Some(5), "{}", text(&out));
    assert!(text(&out).contains("is not empty"));
}

#[test]
fn connector_is_an_alias_and_test_is_not_available_yet() {
    for word in ["connector", "connectors"] {
        let out = vam(&[
            word,
            "validate",
            fixture("marketplace-squrell").to_str().unwrap(),
        ]);
        assert!(out.status.success(), "{word}: {}", text(&out));
    }
    let out = vam(&[
        "connector",
        "test",
        fixture("marketplace-squrell").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("`vam connector test` is not available yet"));
    let out = vam(&["--help"]);
    let help = text(&out);
    assert!(
        help.contains("connectors") && help.contains("[alias: connector]"),
        "{help}"
    );
    assert!(!help.contains("Microsoft 365 connection"), "{help}");
    let out = vam(&["connector", "--help"]);
    let help = text(&out);
    for sub in ["init", "validate", "test"] {
        assert!(help.contains(&format!("  {sub} ")), "{sub}: {help}");
    }
    assert!(!help.contains("Microsoft 365 connection"), "{help}");
}
