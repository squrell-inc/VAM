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

/// Like [`vam`], with this process's `PATH` (a tool's interpreter is found through it).
fn vam_on_path(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vam"))
        .args(args)
        .env_clear()
        .env("HOME", std::env::temp_dir())
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
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
next: vam connector validate {d}   (then `vam connector test {d}`)
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
fn connector_is_an_alias_and_test_skips_a_listing_without_a_test_file() {
    for word in ["connector", "connectors"] {
        let out = vam(&[
            word,
            "validate",
            fixture("marketplace-squrell").to_str().unwrap(),
        ]);
        assert!(out.status.success(), "{word}: {}", text(&out));
    }
    // Every listing is skipped: no case ran, which is not a pass.
    let out = vam(&[
        "connector",
        "test",
        fixture("marketplace-squrell").to_str().unwrap(),
    ]);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(6), "{shown}");
    assert!(
        shown.contains("skip microsoft-365: no listings/microsoft-365/tests/connector-test.json")
            && shown.contains("no case ran")
            && !shown.contains("the key never reached a tool"),
        "{shown}"
    );
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

/// `init` output passes `test` as written; a broken tool prints the gate's words and exits 6.
#[test]
fn test_passes_the_init_output_and_fails_a_broken_tool_in_the_gates_words() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("kit");
    let d = dir.to_str().unwrap();
    assert!(vam(&init_args(d, &["--runtime", "python3"]))
        .status
        .success());
    let out = vam_on_path(&["connector", "test", d]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        text(&out),
        "pass notesdesk_read
1 case(s) passed, 0 failed
the key never reached a tool
"
    );
    assert!(!text(&out).contains("vamkey_"));

    let out = vam_on_path(&[
        "connector",
        "test",
        fixture("connector-kit/broken-tool").to_str().unwrap(),
    ]);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(6), "{shown}");
    assert!(shown.contains("pass note_search"), "{shown}");
    assert!(
        shown.contains("fail note_leak: ")
            && shown.contains("only valid as the Authorization value"),
        "{shown}"
    );
    assert!(
        shown.contains("fail note_wander: ") && shown.contains("declared shape"),
        "{shown}"
    );
    assert!(
        !shown.contains("vamkey_") && !shown.contains("the key never reached a tool"),
        "{shown}"
    );
}

/// A root where no case ran exits 6 and says so; a root with one REST case still passes; a remote
/// MCP listing counts as checked (exit 0, with its skip line), alone or named by `--listing`.
#[test]
fn test_with_no_case_run_exits_6_but_a_checked_mcp_listing_does_not() {
    let t = tempfile::tempdir().unwrap();
    // A root whose one listing has no test file (the kit's own output, the file removed).
    let rest = t.path().join("rest");
    let d = rest.to_str().unwrap();
    assert!(vam(&init_args(d, &["--runtime", "python3"]))
        .status
        .success());
    std::fs::remove_file(rest.join("listings/notesdesk/tests/connector-test.json")).unwrap();
    for json in [false, true] {
        let mut args = vec!["connector", "test", d];
        if json {
            args.insert(0, "--json");
        }
        let out = vam_on_path(&args);
        let shown = text(&out);
        assert_eq!(out.status.code(), Some(6), "json={json}: {shown}");
        assert!(
            shown.contains("no case ran") && !shown.contains("the key never reached a tool"),
            "json={json}: {shown}"
        );
    }
    // Naming the listing says the file is missing, and exits 6 as before.
    let out = vam_on_path(&["connector", "test", d, "--listing", "notesdesk"]);
    assert_eq!(out.status.code(), Some(6), "{}", text(&out));

    // One REST case still passes.
    let ok = t.path().join("ok");
    let d = ok.to_str().unwrap();
    assert!(vam(&init_args(d, &["--runtime", "python3"]))
        .status
        .success());
    let out = vam_on_path(&["connector", "test", d]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains("the key never reached a tool") && !text(&out).contains("no case ran"),
        "{}",
        text(&out)
    );

    // A remote MCP listing was checked: nothing to run, and that is not a failure.
    let mcp = t.path().join("mcp");
    let d = mcp.to_str().unwrap();
    let out = vam(&[
        "connector",
        "init",
        d,
        "--id",
        "bugwatch",
        "--auth",
        "discover",
        "--mcp-url",
        "https://mcp.bugwatch.example/mcp",
        "--domain",
        "kit.community.example",
    ]);
    assert!(out.status.success(), "{}", text(&out));
    for extra in [&[][..], &["--listing", "bugwatch"][..]] {
        let mut args = vec!["connector", "test", d];
        args.extend_from_slice(extra);
        let out = vam_on_path(&args);
        let shown = text(&out);
        assert_eq!(out.status.code(), Some(0), "{extra:?}: {shown}");
        assert!(
            shown.contains("skip bugwatch: a remote MCP connector")
                && shown.contains("no case ran")
                && !shown.contains("the key never reached a tool"),
            "{extra:?}: {shown}"
        );
    }
}
