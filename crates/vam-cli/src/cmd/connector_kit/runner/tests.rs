use super::*;
use vam_spec::connector_kit::TestCase;

const PH: &str = "vamsec_notesdesk_api";
const HOST: &str = "api.notesdesk.example";
const URN: &str = "urn:vam:connector:kit.community.example/community/notesdesk";
const KEY: &str = "vamkey_0123456789abcdef0123456789abcdef";

fn file(routes: &str) -> ConnectorTestFile {
    ConnectorTestFile::parse(&format!(
        r#"{{"hosts":["{HOST}"],"routes":{routes},"cases":[]}}"#
    ))
    .unwrap()
}

fn binding(name: &str, placeholder: &str) -> BindingSpec {
    BindingSpec {
        connector: URN.into(),
        name: name.into(),
        placeholder: placeholder.into(),
        hosts: vec![HOST.into()],
        header: None,
    }
}

fn header_binding(name: &str, prefix: &str) -> BindingSpec {
    BindingSpec {
        header: Some(HeaderInject {
            name: name.into(),
            prefix: prefix.into(),
        }),
        ..binding("api", PH)
    }
}

fn read_shape() -> Shape {
    Shape {
        binding: "api".into(),
        host: HOST.into(),
        methods: vec!["GET".into()],
        path_prefixes: vec!["/v1/notes".into()],
        paths: vec![],
    }
}

fn stub_for(
    bindings: Vec<BindingSpec>,
    shapes: Vec<Shape>,
    requires: Option<&str>,
    f: ConnectorTestFile,
) -> Stub {
    let window = Arc::new(CallWindow::new());
    window.open(ActiveCall::new(
        "c1",
        "note_search",
        requires.map(str::to_string),
        shapes,
        Duration::from_secs(60),
    ));
    Stub {
        envelope: Envelope::new(bindings, window),
        file: f,
        credential: KEY.into(),
        events: Mutex::new(Vec::new()),
    }
}

fn stub() -> Stub {
    stub_for(
        vec![binding("api", PH)],
        vec![read_shape()],
        Some(URN),
        file(
            r#"[{"method":"GET","prefix":"/v1/notes","status":200,"body":{"notes":["Printer notes"]}}]"#,
        ),
    )
}

fn request(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> RawRequest {
    let mut h: Vec<(String, String)> = vec![
        ("Host".into(), HOST.into()),
        ("X-Target-Url".into(), url.into()),
    ];
    h.extend(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    RawRequest {
        method: method.into(),
        target: "/".into(),
        headers: h,
        body: body.to_vec(),
    }
}

fn get(url: &str) -> RawRequest {
    request("GET", url, &[("Authorization", PH)], b"")
}

fn text(r: &Reply) -> String {
    String::from_utf8_lossy(&r.body).into_owned()
}

/// The request is refused with the gate's words: a 502 naming `word`, a refusal event, and neither
/// the placeholder nor the credential in the words.
#[track_caller]
fn refused_with(s: &Stub, req: RawRequest, word: &str) {
    let r = s.handle(&req);
    let body = text(&r);
    assert_eq!(r.status, 502, "{body}");
    assert!(body.contains(word), "{word} not in {body}");
    assert!(!body.contains(PH) && !body.contains(KEY), "{body}");
    let events = s.events.lock().unwrap().clone();
    assert!(
        matches!(events.as_slice(), [Event::Refused(t)] if body.ends_with(t.as_str())),
        "{events:?}"
    );
}

#[test]
fn a_request_inside_the_shape_gets_the_mocks_answer() {
    let s = stub();
    for url in [
        "https://api.notesdesk.example/v1/notes",
        "https://api.notesdesk.example/v1/notes?query=printer",
        "https://api.notesdesk.example/v1/notes/7",
    ] {
        let r = s.handle(&get(url));
        assert_eq!(
            (r.status, r.content_type),
            (200, "application/json"),
            "{url}"
        );
        assert!(text(&r).contains("Printer notes"));
    }
    assert_eq!(
        s.events.lock().unwrap().clone(),
        vec![Event::Answered(200); 3]
    );
}

#[test]
fn a_route_the_mock_lacks_is_404_behind_the_key() {
    let s = stub();
    // Inside the shape (a prefix of /v1/notes), but no route answers a POST: the shape forbids it.
    let r = s.handle(&request(
        "POST",
        "https://api.notesdesk.example/v1/notes",
        &[("Authorization", PH)],
        b"{}",
    ));
    assert_eq!(r.status, 502);
    let wide = Shape {
        methods: vec!["GET".into(), "POST".into()],
        ..read_shape()
    };
    let s = stub_for(vec![binding("api", PH)], vec![wide], Some(URN), file("[]"));
    let r = s.handle(&request(
        "POST",
        "https://api.notesdesk.example/v1/notes",
        &[("Authorization", PH)],
        b"{}",
    ));
    assert_eq!(
        (r.status, text(&r).as_str()),
        (404, r#"{"error":"not found"}"#)
    );
}

#[test]
fn the_mock_needs_the_key_it_was_handed() {
    let f = file(r#"[{"method":"GET","prefix":"/v1","status":200}]"#);
    let h = |auth: &str| HashMap::from([("Authorization".to_string(), auth.to_string())]);
    let bearer = format!("Bearer {KEY}");
    let ask = |headers: &HashMap<String, String>| {
        upstream(&f, HOST, "GET", "/v1/x", headers, "authorization", &bearer).status
    };
    assert_eq!(ask(&h(&format!("Bearer {KEY}"))), 200);
    assert_eq!(ask(&h("Bearer vamkey_other")), 401);
    assert_eq!(ask(&h(KEY)), 401);
    assert_eq!(ask(&h(PH)), 401);
    assert_eq!(ask(&HashMap::new()), 401);
}

#[test]
fn the_credential_is_scrubbed_from_the_answer() {
    let s = stub_for(
        vec![binding("api", PH)],
        vec![read_shape()],
        Some(URN),
        file(&format!(
            r#"[{{"method":"GET","prefix":"/v1/notes","status":200,"body":{{"echo":"{KEY}","again":"Bearer {KEY}"}}}}]"#
        )),
    );
    let r = s.handle(&get("https://api.notesdesk.example/v1/notes"));
    let body = text(&r);
    assert!(!body.contains(KEY), "{body}");
    assert!(body.contains(PH), "{body}");
}

#[test]
fn outside_the_declared_shape_is_refused() {
    let s = stub();
    // a path outside, a path that only starts like the prefix, a dot segment, an encoded dot segment
    for url in [
        "https://api.notesdesk.example/v1/admin/export",
        "https://api.notesdesk.example/v1/notesx",
        "https://api.notesdesk.example/v1/notes/%2e%2e/admin",
        "https://api.notesdesk.example/v1/notes%2Fx",
    ] {
        let s2 = stub();
        refused_with(&s2, get(url), "declared shape");
    }
    // a method outside
    refused_with(
        &s,
        request(
            "DELETE",
            "https://api.notesdesk.example/v1/notes",
            &[("Authorization", PH)],
            b"",
        ),
        "declared shape",
    );
}

#[test]
fn another_host_is_outside_the_shape() {
    refused_with(
        &stub(),
        get("https://api.elsewhere.example/v1/notes"),
        "declared shape",
    );
    // a subdomain, a trailing dot and another port are not the pinned host
    refused_with(
        &stub(),
        get("https://eu.api.notesdesk.example/v1/notes"),
        "declared shape",
    );
}

#[test]
fn the_words_name_the_tool_and_its_shape() {
    let s = stub();
    let r = s.handle(&get("https://api.notesdesk.example/v1/admin"));
    let body = text(&r);
    assert!(body.contains("note_search"), "{body}");
    assert!(
        body.contains("GET api.notesdesk.example /v1/notes"),
        "{body}"
    );
}

#[test]
fn the_placeholder_is_only_valid_as_the_authorization_value() {
    let word = "only valid as the Authorization value";
    // in the query string
    refused_with(
        &stub(),
        request(
            "GET",
            &format!("https://api.notesdesk.example/v1/notes?key={PH}"),
            &[],
            b"",
        ),
        word,
    );
    // in the body
    refused_with(
        &stub(),
        request(
            "POST",
            "https://api.notesdesk.example/v1/notes",
            &[("Authorization", PH)],
            PH.as_bytes(),
        ),
        word,
    );
    // in another header
    refused_with(
        &stub(),
        request(
            "GET",
            "https://api.notesdesk.example/v1/notes",
            &[("Authorization", PH), ("X-Api-Key", PH)],
            b"",
        ),
        word,
    );
    // in a header name
    refused_with(
        &stub(),
        request(
            "GET",
            "https://api.notesdesk.example/v1/notes",
            &[("Authorization", PH), (PH, "x")],
            b"",
        ),
        word,
    );
    // an Authorization that is not exactly the placeholder
    refused_with(
        &stub(),
        request(
            "GET",
            "https://api.notesdesk.example/v1/notes",
            &[("Authorization", &format!("Bearer {PH}x"))],
            b"",
        ),
        word,
    );
    // and in no Authorization header at all
    refused_with(
        &stub(),
        request(
            "GET",
            "https://api.notesdesk.example/v1/notes",
            &[("X-Api-Key", PH)],
            b"",
        ),
        word,
    );
}

#[test]
fn bearer_and_bare_placeholders_are_both_the_authorization_value() {
    for auth in [
        PH.to_string(),
        format!("Bearer {PH}"),
        format!("bearer {PH}"),
    ] {
        let r = stub().handle(&request(
            "GET",
            "https://api.notesdesk.example/v1/notes",
            &[("Authorization", &auth)],
            b"",
        ));
        assert_eq!(r.status, 200, "{auth}");
    }
}

#[test]
fn two_placeholders_are_refused() {
    let s = stub_for(
        vec![
            binding("api", PH),
            binding("files", "vamsec_notesdesk_files"),
        ],
        vec![read_shape()],
        Some(URN),
        file("[]"),
    );
    refused_with(
        &s,
        request(
            "GET",
            "https://api.notesdesk.example/v1/notes",
            &[("Authorization", PH), ("X-Other", "vamsec_notesdesk_files")],
            b"",
        ),
        "more than one connection placeholder",
    );
}

#[test]
fn a_tool_with_no_egress_shape_is_refused() {
    let none = stub_for(vec![binding("api", PH)], vec![], Some(URN), file("[]"));
    refused_with(
        &none,
        get("https://api.notesdesk.example/v1/notes"),
        "declares no egress shape",
    );
    let no_connector = stub_for(
        vec![binding("api", PH)],
        vec![read_shape()],
        None,
        file("[]"),
    );
    refused_with(
        &no_connector,
        get("https://api.notesdesk.example/v1/notes"),
        "declares no egress shape",
    );
}

#[test]
fn a_tool_of_another_connector_or_binding_is_outside_the_shape() {
    let other = stub_for(
        vec![binding("api", PH)],
        vec![read_shape()],
        Some("urn:vam:connector:kit.community.example/community/other"),
        file("[]"),
    );
    refused_with(
        &other,
        get("https://api.notesdesk.example/v1/notes"),
        "declared shape",
    );
    let wrong_binding = stub_for(
        vec![binding("api", PH)],
        vec![Shape {
            binding: "files".into(),
            ..read_shape()
        }],
        Some(URN),
        file("[]"),
    );
    refused_with(
        &wrong_binding,
        get("https://api.notesdesk.example/v1/notes"),
        "declared shape",
    );
}

#[test]
fn a_url_the_gate_cannot_read_as_one_destination_is_refused() {
    for (url, why) in [
        ("http://api.notesdesk.example/v1/notes", "https only"),
        (
            "https://user@api.notesdesk.example/v1/notes",
            "no credentials in the URL",
        ),
        (
            "https://api.notesdesk.example:8443/v1/notes",
            "host port not allowed",
        ),
        (
            "https://api.notesdesk.example./v1/notes",
            "host form not allowed",
        ),
        (
            "https://api.notesdesk.example/v1/../notes",
            "not in canonical form",
        ),
    ] {
        refused_with(&stub(), get(url), why);
    }
}

#[test]
fn a_placeholder_no_binding_declares_is_refused() {
    refused_with(
        &stub(),
        request(
            "GET",
            "https://api.notesdesk.example/v1/notes",
            &[("Authorization", "vamsec_other_api")],
            b"",
        ),
        "no binding of this connector declares",
    );
}

#[test]
fn a_request_with_no_credential_is_401_and_another_host_is_not_answered() {
    let s = stub();
    let r = s.handle(&request(
        "GET",
        "https://api.notesdesk.example/v1/notes",
        &[],
        b"",
    ));
    assert_eq!(r.status, 401);
    let r = s.handle(&request("GET", "https://example.org/", &[], b""));
    assert_eq!(r.status, 502);
    let events = s.events.lock().unwrap().clone();
    assert_eq!(events[0], Event::Answered(401));
    assert!(
        matches!(&events[1], Event::Unanswered(w) if w.contains("example.org") && w.contains("sends nothing off this machine")),
        "{events:?}"
    );
}

#[test]
fn an_admitted_host_the_test_file_does_not_declare_is_not_answered() {
    let two = stub_for(
        vec![BindingSpec {
            hosts: vec![HOST.into(), "eu.api.notesdesk.example".into()],
            ..binding("api", PH)
        }],
        vec![
            read_shape(),
            Shape {
                host: "eu.api.notesdesk.example".into(),
                ..read_shape()
            },
        ],
        Some(URN),
        file("[]"),
    );
    let r = two.handle(&get("https://eu.api.notesdesk.example/v1/notes"));
    assert_eq!(r.status, 502);
    assert!(
        text(&r).contains("is not one of the test file's hosts"),
        "{}",
        text(&r)
    );
}

#[test]
fn a_target_in_the_request_line_or_the_host_header_is_read_like_the_guest_proxy_does() {
    let s = stub();
    let r = s.handle(&RawRequest {
        method: "GET".into(),
        target: "https://api.notesdesk.example/v1/notes".into(),
        headers: vec![("Authorization".into(), PH.into())],
        body: vec![],
    });
    assert_eq!(r.status, 200);
    // `http://` + Host is not https: the gate refuses it
    let r = s.handle(&RawRequest {
        method: "GET".into(),
        target: "/v1/notes".into(),
        headers: vec![
            ("Host".into(), HOST.into()),
            ("Authorization".into(), PH.into()),
        ],
        body: vec![],
    });
    assert_eq!(r.status, 502);
    assert!(text(&r).contains("https only"), "{}", text(&r));
    let r = s.handle(&RawRequest {
        method: "GET".into(),
        target: "/".into(),
        headers: vec![],
        body: vec![],
    });
    assert_eq!(r.status, 502);
}

// ---- judging a case -----------------------------------------------------------------------------

fn case(expect: Expect, contains: Option<&str>) -> TestCase {
    TestCase {
        tool: "note_search".into(),
        input: json!({}),
        expect,
        contains: contains.map(str::to_string),
    }
}

fn ran(status: Option<i32>, stdout: &str, stderr: &str) -> Result<Ran, String> {
    Ok(Ran {
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        status,
    })
}

const DECLARED: &str = "Connection credential refused: this request is outside the declared shape of tool 'note_search' (GET api.notesdesk.example /v1/notes).";

#[test]
fn an_ok_case_needs_an_exit_zero_the_text_and_no_refusal() {
    let ok = case(Expect::Ok, Some("Printer"));
    assert_eq!(
        judge(
            &ok,
            &[Event::Answered(200)],
            ran(Some(0), "Printer notes", "")
        ),
        Ok(())
    );
    assert_eq!(
        judge(&ok, &[Event::Answered(200)], ran(Some(0), "nothing", "")),
        Err("the tool's output does not contain `Printer`".into())
    );
    // no `contains`: an exit zero is enough
    assert_eq!(
        judge(&case(Expect::Ok, None), &[], ran(Some(0), "", "")),
        Ok(())
    );
    // the gate's words come first, whatever the tool printed
    assert_eq!(
        judge(
            &ok,
            &[Event::Refused(DECLARED.into())],
            ran(Some(0), "Printer notes", "")
        ),
        Err(DECLARED.into())
    );
    // a host the mock cannot answer says why
    assert_eq!(
        judge(
            &ok,
            &[Event::Unanswered("no host".into())],
            ran(Some(1), "", "")
        ),
        Err("no host".into())
    );
    let e = judge(
        &ok,
        &[Event::Answered(404)],
        ran(Some(1), "", "warn\nnotesdesk-error: answered 404\n"),
    )
    .unwrap_err();
    assert_eq!(
        e,
        "the tool exited with status 1: notesdesk-error: answered 404"
    );
    assert_eq!(
        judge(&ok, &[], ran(None, "", "")),
        Err("the tool did not finish in time".into())
    );
    assert_eq!(
        judge(&ok, &[], Err("cannot start `python3`: not found".into())),
        Err("cannot start `python3`: not found".into())
    );
}

#[test]
fn a_refused_case_needs_the_gates_refusal() {
    let refused = case(Expect::Refused, Some("declared shape"));
    assert_eq!(
        judge(
            &refused,
            &[Event::Refused(DECLARED.into())],
            ran(Some(1), "", "")
        ),
        Ok(())
    );
    assert_eq!(
        judge(
            &case(Expect::Refused, None),
            &[Event::Refused(DECLARED.into())],
            ran(Some(0), "", "")
        ),
        Ok(())
    );
    let e = judge(
        &case(Expect::Refused, Some("canonical")),
        &[Event::Refused(DECLARED.into())],
        ran(Some(1), "", ""),
    )
    .unwrap_err();
    assert!(
        e.starts_with("the refusal does not say `canonical`: Connection credential refused"),
        "{e}"
    );
    let e = judge(&refused, &[Event::Answered(200)], ran(Some(0), "x", "")).unwrap_err();
    assert_eq!(
        e,
        "expected the gate to refuse the tool's request, but it was admitted and answered 200"
    );
    let e = judge(&refused, &[], ran(Some(0), "x", "")).unwrap_err();
    assert_eq!(
        e,
        "expected the gate to refuse the tool's request, but the tool made none"
    );
}

#[test]
fn the_credential_is_found_in_output_and_in_files_in_every_form() {
    use base64::Engine as _;
    let redactor = SecretRedactor::with_replacement(&[KEY.as_bytes()], "[credential]");
    assert!(holds(&redactor, format!("x {KEY} y").as_bytes()));
    assert!(holds(&redactor, format!("Bearer {KEY}").as_bytes()));
    let b64 = base64::engine::general_purpose::STANDARD.encode(KEY);
    assert!(holds(&redactor, b64.as_bytes()));
    assert!(!holds(&redactor, b"vamkey_ and Printer notes"));
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("work/deep")).unwrap();
    fs::write(dir.path().join("work/clean.txt"), "Printer notes").unwrap();
    assert_eq!(file_holding(dir.path(), dir.path(), &redactor), None);
    fs::write(
        dir.path().join("work/deep/cache.json"),
        format!("{{\"k\":\"{KEY}\"}}"),
    )
    .unwrap();
    let found = file_holding(dir.path(), dir.path(), &redactor).unwrap();
    assert!(
        found.ends_with("cache.json") && !found.contains(KEY),
        "{found}"
    );
}

#[test]
fn a_url_is_split_into_host_and_target() {
    let split = |u: &str| split_url(u).unwrap();
    assert_eq!(
        split("https://API.Example.com/v1/x?q=1#f"),
        ("api.example.com".into(), "/v1/x?q=1#f".into())
    );
    assert_eq!(split("https://a.example"), ("a.example".into(), "/".into()));
    assert_eq!(
        split("http://a.example:8080?x=1"),
        ("a.example".into(), "?x=1".into())
    );
    assert_eq!(split_url("ftp://a.example/"), None);
    assert_eq!(split_url("https:///x"), None);
}

// ---- whole commands, with the tools run -----------------------------------------------------------

fn has(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn init_args(
    dir: &Path,
    auth: super::super::Auth,
    runtime: super::super::Runtime,
) -> super::super::InitArgs {
    super::super::InitArgs {
        dir: dir.display().to_string(),
        id: "notesdesk".into(),
        auth,
        host: Some("api.notesdesk.example".into()),
        mcp_url: None,
        tenant_suffix: None,
        domain: "kit.community.example".into(),
        runtime,
    }
}

fn dir_arg(p: &Path) -> String {
    p.display().to_string()
}

/// The `init` output of each runtime passes `test` as written, and a leftover file in the tree
/// is never touched by it.
#[test]
fn init_output_passes_test_unedited_for_python() {
    use super::super::{init, Auth, Runtime};
    let t = tempfile::tempdir().unwrap();
    for (n, auth) in [Auth::ApiKey, Auth::Oauth].into_iter().enumerate() {
        let root = t.path().join(format!("kit{n}"));
        init(&init_args(&root, auth, Runtime::Python3)).unwrap();
        test(&dir_arg(&root), None, None).unwrap_or_else(|e| panic!("{e:#}"));
        // one listing directory, and by --listing
        test(&dir_arg(&root.join("listings/notesdesk")), None, None).unwrap();
        test(&dir_arg(&root), Some("notesdesk"), None).unwrap();
        let mut seen = Vec::new();
        for entry in walk(&root) {
            seen.push(entry);
        }
        assert!(seen.iter().all(|p| !p.contains(".vam")), "{seen:?}");
    }
}

fn walk(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        out.push(p.display().to_string());
        if p.is_dir() {
            out.extend(walk(&p));
        }
    }
    out
}

#[test]
fn init_output_passes_test_unedited_for_bun() {
    use super::super::{init, Auth, Runtime};
    if !has("bun") {
        eprintln!("bun is not installed: the Bun kit is not run here");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("kit");
    init(&init_args(&root, Auth::ApiKey, Runtime::Bun)).unwrap();
    test(&dir_arg(&root), None, None).unwrap_or_else(|e| panic!("{e:#}"));
}

#[test]
fn init_output_with_a_company_address_passes_test_for_every_runtime_and_subdomain() {
    use super::super::{init, Auth, InitArgs, Runtime};
    let t = tempfile::tempdir().unwrap();
    let mut runtimes = vec![Runtime::Python3];
    if has("bun") {
        runtimes.push(Runtime::Bun);
    }
    for (n, runtime) in runtimes.into_iter().enumerate() {
        for (m, auth) in [Auth::ApiKey, Auth::Oauth].into_iter().enumerate() {
            let root = t.path().join(format!("kit{n}{m}"));
            init(&InitArgs {
                host: Some("{subdomain}.notesdesk.example".into()),
                tenant_suffix: Some("notesdesk.example".into()),
                ..init_args(&root, auth, runtime)
            })
            .unwrap();
            test(&dir_arg(&root), None, None).unwrap_or_else(|e| panic!("{e:#}"));
            // the sample address is the one the test file names; another one has no mock host
            test(&dir_arg(&root), None, Some("demo")).unwrap();
            let e = test(&dir_arg(&root), None, Some("acme")).unwrap_err();
            assert_eq!(crate::output::classify(&e).0, ExitCode::Invalid);
        }
    }
}

#[test]
fn an_mcp_listing_runs_nothing_and_only_its_test_file_is_checked() {
    use super::super::{init, Auth, InitArgs, Runtime};
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("kit");
    init(&InitArgs {
        host: None,
        mcp_url: Some("https://mcp.notesdesk.example/mcp".into()),
        ..init_args(&root, Auth::ApiKey, Runtime::Bun)
    })
    .unwrap();
    test(&dir_arg(&root), None, None).unwrap();
    // a test file that does not parse is a failure even though nothing runs
    let path = root.join("listings/notesdesk/tests/connector-test.json");
    fs::write(&path, r#"{"hosts":["https://mcp.notesdesk.example"]}"#).unwrap();
    let e = test(&dir_arg(&root), None, None).unwrap_err();
    assert_eq!(crate::output::classify(&e).0, ExitCode::Invalid);
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/world/fixtures/connector-kit")
        .join(name)
}

fn copy_to(from: &Path, t: &Path) -> PathBuf {
    let to = t.join("tree");
    copy_tree(from, &to).unwrap();
    to
}

/// The fixture the scenario runs: one tool stays inside its shape, one puts the placeholder in the
/// query string, one reads a path outside its shape; each failure is the gate's words.
#[test]
fn a_broken_tool_fails_in_the_gates_words_and_the_exit_is_6() {
    let t = tempfile::tempdir().unwrap();
    let root = copy_to(&fixture("broken-tool"), t.path());
    let e = test(&dir_arg(&root), None, None).unwrap_err();
    assert_eq!(crate::output::classify(&e).0, ExitCode::Invalid);
    assert!(e.to_string().contains("2 of 3 case(s) failed"), "{e}");
    let (lines, failed) = run_lines(&root);
    assert_eq!(failed, 2);
    let by_tool = |tool: &str| {
        lines
            .iter()
            .find(|l| l.tool == tool)
            .unwrap()
            .failure
            .clone()
    };
    assert_eq!(by_tool("note_search"), None);
    assert!(by_tool("note_leak")
        .unwrap()
        .contains("only valid as the Authorization value"));
    assert!(by_tool("note_wander").unwrap().contains("declared shape"));
}

/// The lines `test` would print for a root, without printing.
fn run_lines(root: &Path) -> (Vec<Line>, usize) {
    let (root, ids) = locate(&dir_arg(root), None).unwrap();
    let mut lines = Vec::new();
    for id in &ids {
        test_listing(&root, id, None, false, &mut |l| lines.push(l), &mut |_| {}).unwrap();
    }
    let failed = lines.iter().filter(|l| l.failure.is_some()).count();
    (lines, failed)
}

/// Editing the fixture's expectations: a refusal the test file expects passes, with the words.
#[test]
fn a_case_that_expects_the_refusal_passes_it() {
    let t = tempfile::tempdir().unwrap();
    let root = copy_to(&fixture("broken-tool"), t.path());
    let path = root.join("listings/notesdesk/tests/connector-test.json");
    let mut doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    doc["cases"] = json!([
        { "tool": "note_search", "expect": "ok", "contains": "Printer notes" },
        { "tool": "note_leak", "expect": "refused", "contains": "only valid as the Authorization value" },
        { "tool": "note_wander", "expect": "refused", "contains": "declared shape" }
    ]);
    fs::write(&path, doc.to_string()).unwrap();
    test(&dir_arg(&root), None, None).unwrap_or_else(|e| panic!("{e:#}"));
    // the wrong word, and a refusal expected of a tool that is admitted
    doc["cases"] = json!([
        { "tool": "note_wander", "expect": "refused", "contains": "only valid as the Authorization value" },
        { "tool": "note_search", "expect": "refused" }
    ]);
    fs::write(&path, doc.to_string()).unwrap();
    let (lines, failed) = run_lines(&root);
    assert_eq!(failed, 2);
    assert!(lines[0]
        .failure
        .as_ref()
        .unwrap()
        .starts_with("the refusal does not say"));
    assert!(lines[1]
        .failure
        .as_ref()
        .unwrap()
        .contains("was admitted and answered 200"));
}

#[test]
fn an_unknown_tool_a_missing_file_and_a_wrong_listing_are_named() {
    let t = tempfile::tempdir().unwrap();
    let root = copy_to(&fixture("broken-tool"), t.path());
    let path = root.join("listings/notesdesk/tests/connector-test.json");
    let mut doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    doc["cases"] = json!([{ "tool": "no_such_tool" }]);
    fs::write(&path, doc.to_string()).unwrap();
    let (lines, _) = run_lines(&root);
    assert_eq!(
        lines[0].failure.as_deref(),
        Some("the listing's skills have no tool named `no_such_tool`")
    );

    fs::remove_file(&path).unwrap();
    // without --listing the listing is skipped; named, it is a failure
    test(&dir_arg(&root), None, None).unwrap();
    let e = test(&dir_arg(&root), Some("notesdesk"), None).unwrap_err();
    assert_eq!(crate::output::classify(&e).0, ExitCode::Invalid);
    let e = test(&dir_arg(&root), Some("nothere"), None).unwrap_err();
    assert_eq!(crate::output::classify(&e).0, ExitCode::Usage);
    let e = test(&dir_arg(&root), None, Some("Not A Label")).unwrap_err();
    assert_eq!(crate::output::classify(&e).0, ExitCode::Usage);
    let e = test(&dir_arg(&root.join("listings")), None, None).unwrap_err();
    assert_eq!(crate::output::classify(&e).0, ExitCode::Invalid);
}

/// A tool that fails, one that never finishes, and one that is the wrong kind of tool.
#[test]
fn a_tool_that_fails_hangs_or_cannot_run_is_a_failure_with_a_reason() {
    use super::super::{init, Auth, Runtime};
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("kit");
    init(&init_args(&root, Auth::ApiKey, Runtime::Python3)).unwrap();
    let skill = root.join("listings/notesdesk/skills/notesdesk-read");
    fs::write(
        skill.join("fail.py"),
        "import sys\nprint('boom: nope', file=sys.stderr)\nsys.exit(3)\n",
    )
    .unwrap();
    fs::write(skill.join("hang.py"), "import time\ntime.sleep(60)\n").unwrap();
    let mut manifest: Value =
        serde_json::from_str(&fs::read_to_string(skill.join("manifest.json")).unwrap()).unwrap();
    let base = manifest["tools"][0].clone();
    let tool = |name: &str, entry: &str, extra: Value| {
        let mut t = base.clone();
        t["name"] = json!(name);
        t["localExec"]["entrypoint"] = json!(entry);
        if let (Some(o), Some(e)) = (t.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        t
    };
    let mut odd = base.clone();
    odd["name"] = json!("odd_command");
    odd["localExec"]["command"] = json!("/bin/sh");
    let mut hostish = base.clone();
    hostish["name"] = json!("no_local_exec");
    hostish.as_object_mut().unwrap().remove("localExec");
    hostish.as_object_mut().unwrap().remove("egress");
    hostish["hostExec"] = json!({ "adapterId": "echo" });
    manifest["tools"] = json!([
        tool("fails", "./fail.py", json!({})),
        tool("hangs", "./hang.py", json!({ "timeout": "1s" })),
        odd,
        hostish
    ]);
    fs::write(skill.join("manifest.json"), manifest.to_string()).unwrap();
    let path = root.join("listings/notesdesk/tests/connector-test.json");
    let mut doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    doc["cases"] = json!([{ "tool": "fails" }, { "tool": "hangs" }, { "tool": "odd_command" }, { "tool": "no_local_exec" }]);
    fs::write(&path, doc.to_string()).unwrap();
    let (lines, failed) = run_lines(&root);
    assert_eq!(
        failed,
        4,
        "{:?}",
        lines.iter().map(|l| &l.failure).collect::<Vec<_>>()
    );
    let why = |i: usize| lines[i].failure.clone().unwrap();
    assert_eq!(why(0), "the tool exited with status 3: boom: nope");
    assert_eq!(why(1), "the tool did not finish in time");
    assert!(
        why(2).contains("`/bin/sh` is not one of bun, node and python3"),
        "{}",
        why(2)
    );
    assert!(why(3).contains("no localExec"), "{}", why(3));
}

/// What the tool is run with: a cleared environment with `PATH`, a scratch `HOME`, the stub as
/// `VAM_EGRESS_ENDPOINT` and the proxy variables the guest sets for a python tool.
#[test]
fn the_tool_runs_with_the_guests_cleared_environment() {
    let t = tempfile::tempdir().unwrap();
    let scratch = t.path();
    for d in ["home", "work", "tmp", "skills/s"] {
        fs::create_dir_all(scratch.join(d)).unwrap();
    }
    fs::write(
        scratch.join("skills/s/env.py"),
        "import json, os, sys\nprint(json.dumps({'env': dict(os.environ), 'cwd': os.getcwd(), 'stdin': sys.stdin.read()}))\n",
    )
    .unwrap();
    let tool = ToolRun {
        name: "env".into(),
        requires: String::new(),
        shapes: vec![],
        command: "python3",
        entrypoint: "./env.py".into(),
        default_args: vec![],
        timeout: Duration::from_secs(20),
        source: PathBuf::new(),
        skill_name: "s".into(),
    };
    let out = run_tool(
        &tool,
        &scratch.join("skills/s"),
        scratch,
        &json!({"query":"x"}),
        4242,
    )
    .unwrap();
    assert_eq!(out.status, Some(0));
    let seen: Value = serde_json::from_slice(&out.stdout).unwrap();
    let env = seen["env"].as_object().unwrap();
    assert_eq!(env["VAM_EGRESS_ENDPOINT"], "http://127.0.0.1:4242");
    assert_eq!(env["HTTP_PROXY"], "http://127.0.0.1:4242");
    assert_eq!(env["HOME"], scratch.join("home").display().to_string());
    assert_eq!(env["TMPDIR"], scratch.join("tmp").display().to_string());
    assert!(env.contains_key("PATH"));
    // nothing else of this process came along
    let allowed = [
        "PATH",
        "HOME",
        "LANG",
        "TMPDIR",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "NO_PROXY",
        "no_proxy",
        "VAM_EGRESS_ENDPOINT",
        "LC_CTYPE",
        "PYTHONCOERCECLOCALE",
    ];
    let extra: Vec<&String> = env
        .keys()
        .filter(|k| !allowed.contains(&k.as_str()))
        .collect();
    assert!(extra.is_empty(), "{extra:?}");
    assert_eq!(
        std::fs::canonicalize(seen["cwd"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(scratch.join("work")).unwrap()
    );
    assert_eq!(seen["stdin"], r#"{"query":"x"}"#);
}

/// A tool of a connector with a company address finds the sample address beside its manifest,
/// as the node writes it (`connector` and `hosts`, no token), and the developer's tree is untouched.
#[test]
fn the_hint_file_holds_the_sample_address_and_no_credential() {
    use super::super::{init, Auth, InitArgs, Runtime};
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("kit");
    init(&InitArgs {
        host: Some("{subdomain}.notesdesk.example".into()),
        tenant_suffix: Some("notesdesk.example".into()),
        ..init_args(&root, Auth::ApiKey, Runtime::Python3)
    })
    .unwrap();
    let skill = root.join("listings/notesdesk/skills/notesdesk-read");
    fs::write(
        skill.join("read.py"),
        "import os, sys\nhere = os.path.dirname(os.path.abspath(__file__))\nsys.stdout.write(open(os.path.join(here, '.vam', 'connector.json')).read())\n",
    )
    .unwrap();
    let path = root.join("listings/notesdesk/tests/connector-test.json");
    let mut doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    doc["cases"] = json!([{ "tool": "notesdesk_read", "contains": "\"demo.notesdesk.example\"" }]);
    fs::write(&path, doc.to_string()).unwrap();
    let (lines, failed) = run_lines(&root);
    assert_eq!(
        failed,
        0,
        "{:?}",
        lines.iter().map(|l| &l.failure).collect::<Vec<_>>()
    );
    assert!(!skill.join(".vam").exists());
}

#[test]
fn a_header_binding_puts_the_key_in_its_named_header_and_the_mock_wants_it_there() {
    let routes = r#"[{"method":"GET","prefix":"/v1/notes","status":200,"body":{"notes":["Printer notes"]}}]"#;
    for (name, prefix, sent) in [
        ("X-Api-Key", "", PH.to_string()),
        ("X-Api-Key", "", format!("  {PH}")),
        ("X-Token", "Token ", format!("Token {PH}")),
        ("X-Token", "Token ", PH.to_string()),
    ] {
        let s = stub_for(
            vec![header_binding(name, prefix)],
            vec![read_shape()],
            Some(URN),
            file(routes),
        );
        let r = s.handle(&request(
            "GET",
            "https://api.notesdesk.example/v1/notes",
            // The guest spells the header its own way: it is replaced, not added to.
            &[(&name.to_ascii_lowercase(), sent.as_str())],
            b"",
        ));
        assert_eq!(r.status, 200, "{name}: {}", text(&r));
        assert!(text(&r).contains("Printer notes"));
    }
    // The upstream mock answers 401 unless the key is in that header with that prefix: an
    // Authorization bearer is no key for a header binding.
    let f = file(routes);
    let with = |h: &[(&str, &str)]| {
        let headers: HashMap<String, String> = h
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        upstream(
            &f,
            HOST,
            "GET",
            "/v1/notes",
            &headers,
            "X-Token",
            &format!("Token {KEY}"),
        )
        .status
    };
    assert_eq!(with(&[("x-token", &format!("Token {KEY}"))]), 200);
    assert_eq!(with(&[("X-Token", KEY)]), 401);
    assert_eq!(with(&[("Authorization", &format!("Bearer {KEY}"))]), 401);
    assert_eq!(with(&[]), 401);
}

#[test]
fn a_header_binding_scrubs_the_key_and_the_prefixed_value_from_the_answer() {
    let s = stub_for(
        vec![header_binding("X-Token", "Token ")],
        vec![read_shape()],
        Some(URN),
        file(&format!(
            r#"[{{"method":"GET","prefix":"/v1/notes","status":200,"body":{{"echo":"Token {KEY}","key":"{KEY}"}}}}]"#
        )),
    );
    let r = s.handle(&request(
        "GET",
        "https://api.notesdesk.example/v1/notes",
        &[("X-Token", &format!("Token {PH}"))],
        b"",
    ));
    let body = text(&r);
    assert_eq!(r.status, 200, "{body}");
    assert!(!body.contains(KEY) && !body.contains("Token "), "{body}");
    assert!(body.contains(PH), "{body}");
}

#[test]
fn a_header_binding_refuses_the_placeholder_anywhere_but_its_header() {
    let word = "only valid as the `X-Api-Key` value";
    let s = || {
        stub_for(
            vec![header_binding("X-Api-Key", "")],
            vec![read_shape()],
            Some(URN),
            file("[]"),
        )
    };
    let url = "https://api.notesdesk.example/v1/notes";
    refused_with(
        &s(),
        request("GET", url, &[("Authorization", PH)], b""),
        word,
    );
    refused_with(
        &s(),
        request("GET", url, &[("X-Api-Key", PH), ("X-Echo", PH)], b""),
        word,
    );
    refused_with(
        &s(),
        request("GET", &format!("{url}?key={PH}"), &[("X-Api-Key", PH)], b""),
        word,
    );
    refused_with(
        &s(),
        request("POST", url, &[("X-Api-Key", PH)], PH.as_bytes()),
        word,
    );
    refused_with(
        &s(),
        request(
            "GET",
            url,
            &[("X-Api-Key", PH), ("x-api-key", "other")],
            b"",
        ),
        word,
    );
}
