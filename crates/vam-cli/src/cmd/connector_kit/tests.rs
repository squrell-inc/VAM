use super::*;
use vam_spec::marketplace::{check_listing, Level};
use vam_spec::package::{check_dir, CheckedPackage};

fn args(dir: &Path, id: &str, auth: Auth) -> InitArgs {
    InitArgs {
        dir: dir.display().to_string(),
        id: id.into(),
        auth,
        host: Some("api.notesdesk.example".into()),
        mcp_url: None,
        tenant_suffix: None,
        domain: "kit.community.example".into(),
        runtime: Runtime::Bun,
    }
}

fn mcp(dir: &Path, auth: Auth) -> InitArgs {
    InitArgs {
        host: None,
        mcp_url: Some("https://mcp.notesdesk.example/mcp".into()),
        ..args(dir, "notesdesk", auth)
    }
}

fn tenant(dir: &Path, auth: Auth) -> InitArgs {
    InitArgs {
        host: Some("{subdomain}.notesdesk.example".into()),
        tenant_suffix: Some("notesdesk.example".into()),
        ..args(dir, "notesdesk", auth)
    }
}

/// What `init` wrote passes the root check (no finding at all, warnings included) and the listing
/// check on the packages it reads back.
fn passes(a: &InitArgs) -> std::path::PathBuf {
    init(a).unwrap_or_else(|e| panic!("{e:#}"));
    let root = Path::new(&a.dir);
    let report = check_marketplace_root(root);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert!(!report.artifacts.is_empty());
    let packages: Vec<CheckedPackage> = report
        .artifacts
        .iter()
        .map(|c| check_dir(&root.join(&c.path), Some(c.kind)).unwrap())
        .collect();
    assert!(check_listing(&a.id, &packages).is_empty());
    let report = check_marketplace_listing(root, &a.id);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    root.to_path_buf()
}

#[test]
fn every_auth_writes_a_root_that_passes_validate() {
    let t = tempfile::tempdir().unwrap();
    let d = |n: &str| t.path().join(n);
    let cases = [
        (
            "api_key REST (bun)",
            args(&d("a"), "notesdesk", Auth::ApiKey),
        ),
        (
            "api_key REST (python3)",
            InitArgs {
                runtime: Runtime::Python3,
                ..args(&d("b"), "notesdesk", Auth::ApiKey)
            },
        ),
        ("api_key MCP", mcp(&d("c"), Auth::ApiKey)),
        ("oauth REST", args(&d("d"), "notesdesk", Auth::Oauth)),
        ("oauth REST, company address", tenant(&d("e"), Auth::Oauth)),
        (
            "api_key REST, company address",
            tenant(&d("f"), Auth::ApiKey),
        ),
        ("discover MCP", mcp(&d("g"), Auth::Discover)),
        (
            "an id with a dash and an underscore",
            args(&d("h"), "my_notes-desk", Auth::ApiKey),
        ),
    ];
    for (what, a) in cases {
        std::panic::catch_unwind(|| passes(&a)).unwrap_or_else(|_| panic!("{what} does not pass"));
    }
}

#[test]
fn the_files_written_follow_the_kit_layout() {
    let t = tempfile::tempdir().unwrap();
    let root = passes(&args(&t.path().join("kit"), "notesdesk", Auth::ApiKey));
    for f in [
        "marketplace.json",
        "listings/notesdesk/connector/connector.json",
        "listings/notesdesk/skills/notesdesk-read/manifest.json",
        "listings/notesdesk/skills/notesdesk-read/SKILLS.md",
        "listings/notesdesk/skills/notesdesk-read/read.ts",
        "listings/notesdesk/agents/notesdesk-assistant/agent.json",
        "listings/notesdesk/tests/connector-test.json",
    ] {
        assert!(root.join(f).is_file(), "{f}");
    }
    let t = tempfile::tempdir().unwrap();
    let root = passes(&InitArgs {
        runtime: Runtime::Python3,
        ..args(&t.path().join("kit"), "notesdesk", Auth::ApiKey)
    });
    assert!(root
        .join("listings/notesdesk/skills/notesdesk-read/read.py")
        .is_file());
    // The tool's shape is the binding host, and the test file reads with the format of vam-spec.
    let manifest =
        fs::read_to_string(root.join("listings/notesdesk/skills/notesdesk-read/manifest.json"))
            .unwrap();
    assert!(manifest.contains("\"host\": \"api.notesdesk.example\""));
    assert!(manifest.contains("\"command\": \"python3\""));
    let test_file =
        fs::read_to_string(root.join("listings/notesdesk/tests/connector-test.json")).unwrap();
    let parsed = ConnectorTestFile::parse(&test_file).unwrap();
    assert_eq!(parsed.hosts, ["api.notesdesk.example"]);
    assert_eq!(parsed.cases[0].tool, "notesdesk_read");
}

#[test]
fn an_mcp_connector_has_no_skill_and_a_company_address_test_host_is_concrete() {
    let t = tempfile::tempdir().unwrap();
    let root = passes(&mcp(&t.path().join("kit"), Auth::ApiKey));
    assert!(!root.join("listings/notesdesk/skills").exists());
    let parsed = ConnectorTestFile::parse(
        &fs::read_to_string(root.join("listings/notesdesk/tests/connector-test.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(parsed.hosts, ["mcp.notesdesk.example"]);
    let t = tempfile::tempdir().unwrap();
    let root = passes(&tenant(&t.path().join("kit"), Auth::Oauth));
    let parsed = ConnectorTestFile::parse(
        &fs::read_to_string(root.join("listings/notesdesk/tests/connector-test.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(parsed.hosts, ["demo.notesdesk.example"]);
}

#[test]
fn no_template_token_is_left_in_any_file() {
    let t = tempfile::tempdir().unwrap();
    for (n, a) in [
        args(&t.path().join("a"), "notesdesk", Auth::ApiKey),
        mcp(&t.path().join("b"), Auth::Discover),
        tenant(&t.path().join("c"), Auth::Oauth),
    ]
    .into_iter()
    .enumerate()
    {
        for (path, text) in plan(&a).unwrap() {
            assert!(!text.contains("{{"), "case {n}: {path}");
        }
    }
}

fn refused(a: &InitArgs) -> (ExitCode, String) {
    let e = init(a).unwrap_err();
    (crate::output::classify(&e).0, e.to_string())
}

#[test]
fn a_non_empty_directory_is_refused_and_one_holding_only_keep_is_not() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("kit");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("notes.txt"), "mine").unwrap();
    let (code, msg) = refused(&args(&dir, "notesdesk", Auth::ApiKey));
    assert_eq!(code, ExitCode::Conflict);
    assert!(msg.contains("is not empty"), "{msg}");
    assert_eq!(fs::read_to_string(dir.join("notes.txt")).unwrap(), "mine");
    assert!(!dir.join("marketplace.json").exists());

    fs::remove_file(dir.join("notes.txt")).unwrap();
    fs::write(dir.join(".keep"), "").unwrap();
    passes(&args(&dir, "notesdesk", Auth::ApiKey));
    // Run again over what it wrote: now the directory is not empty.
    assert_eq!(
        refused(&args(&dir, "notesdesk", Auth::ApiKey)).0,
        ExitCode::Conflict
    );
    // A file is not a directory.
    let file = t.path().join("file");
    fs::write(&file, "").unwrap();
    assert_eq!(
        refused(&args(&file, "notesdesk", Auth::ApiKey)).0,
        ExitCode::Conflict
    );
}

#[test]
fn a_reserved_or_malformed_id_is_refused_in_the_nodes_words_and_nothing_is_written() {
    let t = tempfile::tempdir().unwrap();
    for (id, words) in [
        ("linear", "is a Squrell connector id"),
        ("microsoft-365", "is a Squrell connector id"),
        ("Notes Desk", "Notes Desk"),
        ("../escape", "escape"),
        ("-lead", "-lead"),
    ] {
        let dir = t.path().join("kit");
        let (code, msg) = refused(&args(&dir, id, Auth::ApiKey));
        assert_eq!(code, ExitCode::Invalid, "{id}: {msg}");
        assert!(msg.contains(words), "{id}: {msg}");
        assert!(!dir.exists(), "{id}");
    }
    assert!(!t.path().join("escape").exists());
}

#[test]
fn contradicting_options_are_usage_errors() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("kit");
    let bad = |a: InitArgs, words: &str| {
        let (code, msg) = refused(&a);
        assert_eq!(code, ExitCode::Usage, "{msg}");
        assert!(msg.contains(words), "{msg}");
        assert!(!d.exists());
    };
    bad(
        InitArgs {
            mcp_url: Some("https://mcp.notesdesk.example/mcp".into()),
            ..args(&d, "notesdesk", Auth::ApiKey)
        },
        "exactly one of --host and --mcp-url",
    );
    bad(
        InitArgs {
            host: None,
            ..args(&d, "notesdesk", Auth::ApiKey)
        },
        "exactly one of --host and --mcp-url",
    );
    bad(mcp(&d, Auth::Oauth), "--auth oauth is for a REST API");
    bad(
        args(&d, "notesdesk", Auth::Discover),
        "--auth discover is for a remote MCP server",
    );
    bad(
        InitArgs {
            tenant_suffix: Some("notesdesk.example".into()),
            ..mcp(&d, Auth::ApiKey)
        },
        "--tenant-suffix is for a REST API",
    );
    bad(
        InitArgs {
            tenant_suffix: Some("notesdesk.example".into()),
            ..args(&d, "notesdesk", Auth::ApiKey)
        },
        "pass that as --host",
    );
    bad(
        InitArgs {
            host: Some("{subdomain}.notesdesk.example".into()),
            ..args(&d, "notesdesk", Auth::ApiKey)
        },
        "needs --tenant-suffix",
    );
}

#[test]
fn a_host_the_marketplace_refuses_and_a_bad_domain_write_nothing() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("kit");
    let (code, msg) = refused(&InitArgs {
        host: Some("localhost".into()),
        ..args(&d, "notesdesk", Auth::ApiKey)
    });
    assert_eq!(code, ExitCode::Invalid, "{msg}");
    assert!(!d.exists());
    let (code, msg) = refused(&InitArgs {
        domain: "Not A Domain".into(),
        ..args(&d, "notesdesk", Auth::ApiKey)
    });
    assert_eq!(code, ExitCode::Invalid, "{msg}");
    assert!(msg.contains("urn must be"), "{msg}");
    assert!(!d.exists());
    // A domain the Squrell name could pass for is a finding of the connector's own check.
    let (code, _) = refused(&InitArgs {
        domain: "squrell.community.example".into(),
        ..args(&d, "notesdesk", Auth::ApiKey)
    });
    assert_eq!(code, ExitCode::Invalid);
    assert!(!d.exists());
}

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/world/fixtures")
        .join(name)
        .display()
        .to_string()
}

#[test]
fn validate_takes_a_root_or_one_listing_and_names_what_is_wrong() {
    // The squrell root passes (its one warning is the fixture's own); one listing of a root is
    // checked alone.
    validate(&fixture("marketplace-squrell"), false).unwrap();
    validate(
        &fixture("marketplace-squrell/listings/microsoft-365"),
        false,
    )
    .unwrap();
    // The broken listing fails as a listing and as a root, with exit 6.
    for dir in [
        fixture("connector-kit/broken-file"),
        fixture("connector-kit/broken-file/listings/kit-broken"),
    ] {
        let e = validate(&dir, false).unwrap_err();
        assert_eq!(crate::output::classify(&e).0, ExitCode::Invalid, "{dir}");
    }
    // A directory that is neither.
    let t = tempfile::tempdir().unwrap();
    let e = validate(&t.path().display().to_string(), false).unwrap_err();
    assert_eq!(crate::output::classify(&e).0, ExitCode::Invalid);
    assert!(e.to_string().contains("neither a marketplace root"), "{e}");
}

#[test]
fn a_warning_fails_only_with_strict() {
    let t = tempfile::tempdir().unwrap();
    let root = passes(&args(&t.path().join("kit"), "notesdesk", Auth::ApiKey));
    fs::remove_file(root.join("listings/notesdesk/tests/connector-test.json")).unwrap();
    let dir = root.display().to_string();
    let report = check_marketplace_root(&root);
    assert_eq!(report.errors(), 0);
    assert!(report
        .findings
        .iter()
        .any(|f| f.level == Level::Warning && f.message.contains("connector-test.json")));
    validate(&dir, false).unwrap();
    let e = validate(&dir, true).unwrap_err();
    assert_eq!(crate::output::classify(&e).0, ExitCode::Invalid);
}

#[test]
fn test_says_it_is_not_available_yet() {
    let e = test("anywhere").unwrap_err();
    assert_eq!(crate::output::classify(&e).0, ExitCode::Failure);
    assert!(e.to_string().contains("not available yet"), "{e}");
}

#[test]
fn display_names_come_from_the_id() {
    assert_eq!(display_name("notesdesk"), "Notesdesk");
    assert_eq!(display_name("my_api-2"), "My Api 2");
    assert_eq!(
        url_host("https://mcp.a.example:8443/mcp?x=1"),
        "mcp.a.example"
    );
}
