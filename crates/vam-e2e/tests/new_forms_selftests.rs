//! Self-tests of the runner forms for secrets, skills, floods, paging and message size: against a
//! fake vamd, no real guest, no real vamd.

mod support;

use base64::Engine as _;
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;
use support::{Fake, FakeVamd, Plan, Throttle};
use vam_e2e::api::Api;
use vam_e2e::catalog::Catalog;
use vam_e2e::driver::{run_scenario, ScenarioResult};
use vam_e2e::engine::{Cfg, Engine};
use vam_e2e::runner::{Outcome, Report, RunTag};
use vam_e2e::scenario::{Class, Scenario};
use vam_e2e::vamd::Vamd;

const ACTIVE: &str = "status: active\n";
const SECRET_GIVEN: &str = "[org-admin, api-token, skill-host-fixtures, agent-with-host-fixtures]";

fn pieces() -> Vec<vam_e2e::world::WorldPiece> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e");
    Catalog::load(&root)
        .pieces
        .into_iter()
        .map(|(_, w)| w)
        .collect()
}

fn scen(given: &str, extra: &str) -> Scenario {
    let text =
        format!("id: fixture\ntitle: t\ntags: [chat]\nneeds: process\ngiven: {given}\n{extra}");
    serde_yaml::from_str(&text).unwrap()
}

fn fast() -> Cfg {
    let mut c = Cfg::new(RunTag("tg1".into()));
    c.poll = Duration::from_millis(5);
    c.settle = Duration::from_millis(150);
    c.step_budget = Duration::from_secs(2);
    c.scenario_budget = Duration::from_secs(20);
    c.health_timeout = Duration::from_secs(3);
    c
}

fn make(fake: &std::sync::Arc<Fake>) -> Engine {
    *fake.guest_mode.lock().unwrap() = Some("process".into());
    let mut v = FakeVamd::new(fake.clone());
    v.start().unwrap();
    let api = Api::new(&v.base_url(), "key", "tg1");
    let mut e = Engine::new(api, Box::new(v), fast(), pieces());
    e.prepare();
    e
}

fn go(fake: &std::sync::Arc<Fake>, s: &Scenario) -> ScenarioResult {
    let mut e = make(fake);
    run_scenario(&mut e, s)
}

fn failed(r: &ScenarioResult) -> (String, Class) {
    match &r.outcome {
        Outcome::Failed { position, class } => (position.clone(), *class),
        Outcome::Passed => panic!("expected a failure, it passed"),
    }
}

/// The value the engine PUT as the secret: read from the fake's request log, so a test can
/// assert it never appears in a failure.
fn secret_value(fake: &Fake) -> String {
    fake.requests_to("PUT", "/v1/secrets")[0]["value"]
        .as_str()
        .unwrap()
        .to_string()
}

fn thread_with(role: &str, text: &str) -> String {
    json!({"integrity": "ok", "records": [
        {"seq": 1, "kind": "message", "message": {"role": role, "parts": [{"type": "text", "text": text}]}}
    ]})
    .to_string()
}

fn set_file(fake: &Fake, key: &str, body: String) {
    fake.files.lock().unwrap().insert(key.to_string(), body);
}

fn chat_then(then: &str) -> String {
    format!("when:\n  - chat: \"e2e-step:forms go\"\nthen:\n  - {then}\n{ACTIVE}")
}

// ---- world pieces: secret and skill ----------------------------------------------------------

#[test]
fn a_secret_piece_puts_an_org_secret_and_never_prints_it() {
    let fake = Fake::new();
    let s = scen(SECRET_GIVEN, &chat_then("expect: { run: succeeded }"));
    let r = go(&fake, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let puts = fake.requests_to("PUT", "/v1/secrets");
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0]["bindingId"], "api-token-fixture-tg1");
    assert_eq!(puts[0]["scope"], 1);
    let value = secret_value(&fake);
    assert!(value.len() >= 32 && value.contains('"') && value.contains('/'));
    assert!(!format!("{} {}", r.expected, r.observed).contains(&value));
}

#[test]
fn a_skill_piece_publishes_a_package_and_the_agent_names_it() {
    let fake = Fake::new();
    let s = scen(SECRET_GIVEN, &chat_then("expect: { run: succeeded }"));
    let r = go(&fake, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);

    // The package: a tar.gz with SKILLS.md and manifest.json at its root.
    let packages = fake.packages.lock().unwrap().clone();
    assert_eq!(packages.len(), 1);
    let body = &packages[0];
    let start = body.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let end = body.windows(4).rposition(|w| w == b"\r\n--").unwrap();
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&body[start..end]));
    let mut files = std::collections::BTreeMap::new();
    for e in ar.entries().unwrap() {
        let mut e = e.unwrap();
        let mut t = String::new();
        e.read_to_string(&mut t).unwrap();
        files.insert(e.path().unwrap().to_string_lossy().to_string(), t);
    }
    assert_eq!(
        files.keys().cloned().collect::<Vec<_>>(),
        ["SKILLS.md", "manifest.json"]
    );
    let manifest: Value = serde_json::from_str(&files["manifest.json"]).unwrap();
    // The skill's name carries the scenario but no run tag: a mock script spells it out.
    let urn = "urn:vam:skill:local.vam.dev/e2e/skill-host-fixtures-fixture";
    assert_eq!(manifest["resourceId"]["urn"], urn);
    // The secret the tool requires is the one the secret piece put.
    assert_eq!(
        manifest["tools"][0]["required_secrets"][0]["name"],
        "api-token-fixture-tg1"
    );
    assert_eq!(
        manifest["tools"][0]["host_exec"]["adapter_id"],
        "secret_forms"
    );

    // Released as 1.0.0.
    let releases = fake.requests_to("POST", "/v1/skills/packages/");
    assert_eq!(releases[0]["version"]["major"], 1);

    // The agent refs that URN and may use its tools.
    let agents = fake.requests_to("PUT", "/v1/agents");
    let spec = &agents[0]["spec"];
    assert_eq!(spec["skillRefs"][0]["resourceId"]["urn"], urn);
    let rules = spec["permissions"]["tools"]["rules"].to_string();
    assert!(
        rules.contains("skill:skill-host-fixtures-fixture/*"),
        "{rules}"
    );
    assert!(rules.contains("builtin:load_skill"), "{rules}");
}

// ---- secret-absent ---------------------------------------------------------------------------

fn clean_run(fake: &Fake) {
    set_file(
        fake,
        "sp_1/threads/messages",
        thread_with("tool", "nothing here"),
    );
    set_file(
        fake,
        "receipt",
        json!({"receipt": {"isolation": {"summary": "x"}}}).to_string(),
    );
}

#[test]
fn secret_absent_passes_when_no_form_is_left_behind() {
    let fake = Fake::new();
    clean_run(&fake);
    let s = scen(
        SECRET_GIVEN,
        &chat_then("expect: { secret-absent: { secret: api-token } }"),
    );
    let r = go(&fake, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
}

#[test]
fn secret_absent_names_the_form_and_the_place_but_never_the_value() {
    let fake = Fake::new();
    clean_run(&fake);
    // The thread carries the secret base64-encoded; the fake fills the placeholder in.
    set_file(
        &fake,
        "sp_1/threads/messages",
        thread_with("tool", "token {{SECRET_BASE64}} end"),
    );
    let s = scen(
        SECRET_GIVEN,
        &chat_then("expect: { secret-absent: { secret: api-token } }"),
    );
    let r = go(&fake, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
    assert!(r.observed.contains("base64 at offset 0"), "{}", r.observed);
    assert!(r.observed.contains("thread messages"), "{}", r.observed);
    let value = secret_value(&fake);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&value);
    for text in [&r.observed, &r.expected] {
        assert!(!text.contains(&value) && !text.contains(&b64), "{text}");
    }
}

#[test]
fn secret_absent_finds_a_split_secret_by_its_run() {
    let fake = Fake::new();
    clean_run(&fake);
    set_file(
        &fake,
        "sp_1/threads/messages",
        thread_with("tool", "{{SECRET_FIRST_HALF}}\nthen {{SECRET_SECOND_HALF}}"),
    );
    let s = scen(
        SECRET_GIVEN,
        &chat_then("expect: { secret-absent: { secret: api-token } }"),
    );
    let r = go(&fake, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
    assert!(r.observed.contains("run of 12 or more"), "{}", r.observed);
}

#[test]
fn secret_absent_looks_in_the_receipt_and_the_workspace_too() {
    let fake = Fake::new();
    clean_run(&fake);
    set_file(
        &fake,
        "receipt",
        json!({"modelAppendix": {"text": "{{SECRET_HEX}}"}}).to_string(),
    );
    let s = scen(
        SECRET_GIVEN,
        &chat_then("expect: { secret-absent: { secret: api-token } }"),
    );
    let r = go(&fake, &s);
    assert_eq!(failed(&r).1, Class::Product);
    assert!(
        r.observed.contains("hex (lower) form is in the receipt"),
        "{}",
        r.observed
    );

    let fake = Fake::new();
    clean_run(&fake);
    set_file(
        &fake,
        "sp_1/files",
        json!({"files": [{"path": "workspace/out.txt"}]}).to_string(),
    );
    set_file(&fake, "sp_1/workspace/out.txt", "saved: {{SECRET}}".into());
    let r = go(&fake, &s);
    assert_eq!(failed(&r).1, Class::Product);
    assert!(
        r.observed
            .contains("raw form is in the workspace file workspace/out.txt"),
        "{}",
        r.observed
    );
}

#[test]
fn secret_absent_looks_in_run_events() {
    let fake = Fake::new();
    clean_run(&fake);
    fake.plan(Plan {
        states: vec!["RUN_STATE_RUNNING", "RUN_STATE_SUCCEEDED"],
        events: vec![(
            "vam.v1.tool.completed",
            json!({"text": "{{SECRET_BASE64}}"}),
        )],
        ..Default::default()
    });
    let s = scen(
        SECRET_GIVEN,
        &chat_then("expect: { secret-absent: { secret: api-token } }"),
    );
    let r = go(&fake, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
    assert!(
        r.observed
            .contains("base64 at offset 0 form is in the run events"),
        "{}",
        r.observed
    );
}

// ---- thread maxBytes -------------------------------------------------------------------------

#[test]
fn thread_max_bytes_bounds_the_matched_message() {
    let big = format!(
        "{}\n[truncated: kept 61000 of 200000 bytes]",
        "x".repeat(70_000)
    );
    let small = format!(
        "{}\n[truncated: kept 61000 of 200000 bytes]",
        "x".repeat(100)
    );
    let step =
        "expect: { thread: { role: tool, contains: \"[truncated: kept\", maxBytes: 65536 } }";
    let s = scen("[org-admin, agent-plain]", &chat_then(step));

    let fake = Fake::new();
    set_file(&fake, "sp_1/threads/messages", thread_with("tool", &small));
    assert_eq!(go(&fake, &s).report, Report::Pass);

    let fake = Fake::new();
    set_file(&fake, "sp_1/threads/messages", thread_with("tool", &big));
    let r = go(&fake, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
    assert!(
        r.observed.contains("70"),
        "the sizes are in the observation: {}",
        r.observed
    );
    assert!(r.expected.contains("at most 65536 bytes"), "{}", r.expected);
}

// ---- flood -----------------------------------------------------------------------------------

fn throttle(allow: u32, retry_after: bool) -> Throttle {
    Throttle {
        path: "/v1/node",
        bearer_prefix: "secret-of-flooder",
        allow,
        window: Duration::from_millis(800),
        served: 0,
        first_limited: None,
        retry_after,
    }
}

const FLOOD_GIVEN: &str = "[org-admin, { piece: org-member, as: flooder }]";

fn flood_scenario(then: &str) -> Scenario {
    scen(
        FLOOD_GIVEN,
        &format!(
            "when:\n  - flood: {{ as: flooder, path: /v1/node, count: 300 }}\nthen:\n  - {then}\n{ACTIVE}"
        ),
    )
}

#[test]
fn a_limited_flood_is_seen_with_retry_after_and_recovers() {
    let fake = Fake::new();
    *fake.throttle.lock().unwrap() = Some(throttle(100, true));
    let s = flood_scenario(
        "expect: { flood: { rateLimited: { atLeast: 20 }, retryAfter: present, recovers: true } }",
    );
    let r = go(&fake, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    // The flood used the flooder's key only, 300 times, then once more to check recovery.
    let bearers = fake.bearers("GET", "/v1/node");
    let flooder = bearers
        .iter()
        .filter(|b| b.starts_with("secret-of-flooder"))
        .count();
    assert_eq!(flooder, 301, "{bearers:?}");
}

#[test]
fn a_node_that_does_not_limit_fails_the_flood_expect_as_product() {
    let fake = Fake::new();
    let s = flood_scenario("expect: { flood: { rateLimited: { atLeast: 20 } } }");
    let r = go(&fake, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
    assert!(r.observed.contains("0 answered 429"), "{}", r.observed);
    assert!(r.observed.contains("300 x 200"), "{}", r.observed);
}

#[test]
fn a_429_without_retry_after_fails_retry_after_present() {
    let fake = Fake::new();
    *fake.throttle.lock().unwrap() = Some(throttle(100, false));
    let s = flood_scenario("expect: { flood: { retryAfter: present } }");
    let r = go(&fake, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
    assert!(r.observed.contains("no Retry-After"), "{}", r.observed);
}

#[test]
fn a_key_that_stays_limited_does_not_recover() {
    let fake = Fake::new();
    let mut t = throttle(100, true);
    t.window = Duration::from_secs(60);
    *fake.throttle.lock().unwrap() = Some(t);
    let s = flood_scenario("expect: { flood: { recovers: true } }");
    let r = go(&fake, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
    assert!(r.observed.contains("answered 429"), "{}", r.observed);
}

#[test]
fn other_keys_are_not_flooded_by_the_step() {
    let fake = Fake::new();
    *fake.throttle.lock().unwrap() = Some(throttle(100, true));
    let s = scen(
        FLOOD_GIVEN,
        "when:\n  - flood: { as: flooder, path: /v1/node, count: 300 }\nthen:\n  - expect: { node: { fields: { status.guestMode: process } } }\nstatus: active\n",
    );
    assert_eq!(go(&fake, &s).report, Report::Pass);
}

// ---- approval paging -------------------------------------------------------------------------

#[test]
fn approvals_are_read_across_pages() {
    let fake = Fake::new();
    let item = |run: &str| {
        json!({"claims": {"requestId": {"urn": format!("req-{run}")}, "runId": {"urn": run},
        "stepId": "confirm", "kind": "APPROVAL_KIND_TOOL_CALL", "targetToolName": "write_file"}})
    };
    // The run's own approval is on the second page only.
    *fake.approval_pages.lock().unwrap() = vec![
        json!({"approvals": [item("urn:vam:run:local.vam.dev/acme/other")]}),
        json!({"approvals": [item("urn:vam:run:local.vam.dev/acme/run-1")]}),
    ];
    fake.plan(Plan {
        states: vec!["RUN_STATE_RUNNING", "RUN_STATE_PAUSED"],
        ..Default::default()
    });
    let s = scen(
        "[org-admin, agent-plain]",
        "when:\n  - chat: \"e2e-step:ask w\"\nthen:\n  - expect: { approval: pending, tool: write_file }\nstatus: active\n",
    );
    let r = go(&fake, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let asked = fake.requests_to("GET", "/v1/approvals");
    assert!(asked.len() >= 2, "the second page was asked for");
}
