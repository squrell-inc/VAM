//! Self-tests of the runner engine against a fake vamd: no real guest, no real vamd.

mod support;

use serde_json::{json, Value};
use std::time::{Duration, Instant};
use support::{Fake, FakeVamd, Plan};
use vam_e2e::api::Api;
use vam_e2e::catalog::Catalog;
use vam_e2e::driver::{run_scenario, ScenarioResult};
use vam_e2e::engine::{Cfg, Engine};
use vam_e2e::runner::{Outcome, Report, RunTag};
use vam_e2e::scenario::{Class, Scenario};
use vam_e2e::vamd::Vamd;

fn pieces() -> Vec<vam_e2e::world::WorldPiece> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e");
    let mut all: Vec<_> = Catalog::load(&root)
        .pieces
        .into_iter()
        .map(|(_, w)| w)
        .collect();
    // A second identity, for the self-tests only (the catalog's own `org-member` is a later wave).
    all.push(serde_yaml::from_str("piece: org-member\nkind: org\nrole: Member\n").unwrap());
    all
}

fn scen(extra: &str) -> Scenario {
    scen_given("[org-admin, agent-plain]", extra)
}

fn scen_given(given: &str, extra: &str) -> Scenario {
    let text =
        format!("id: fixture\ntitle: t\ntags: [chat]\nneeds: process\ngiven: {given}\n{extra}");
    serde_yaml::from_str(&text).unwrap()
}

fn fast() -> Cfg {
    let mut c = Cfg::new(RunTag("tg1".into()));
    c.poll = Duration::from_millis(5);
    c.settle = Duration::from_millis(150);
    c.step_budget = Duration::from_secs(2);
    c.scenario_budget = Duration::from_secs(10);
    c.health_timeout = Duration::from_secs(3);
    c
}

fn make(fake: &std::sync::Arc<Fake>, cfg: Cfg, mode: Option<&str>) -> Engine {
    *fake.guest_mode.lock().unwrap() = mode.map(String::from);
    let mut v = FakeVamd::new(fake.clone());
    v.start().unwrap();
    let api = Api::new(&v.base_url(), "key", "tg1");
    let mut e = Engine::new(api, Box::new(v), cfg, pieces());
    e.prepare();
    e
}

fn go(e: &mut Engine, s: &Scenario) -> ScenarioResult {
    run_scenario(e, s)
}

fn failed(r: &ScenarioResult) -> (String, Class) {
    match &r.outcome {
        Outcome::Failed { position, class } => (position.clone(), *class),
        Outcome::Passed => panic!("expected a failure, it passed"),
    }
}

const ACTIVE: &str = "status: active\n";

#[test]
fn prepare_allows_the_mock_model() {
    let fake = Fake::new();
    let _e = make(&fake, fast(), Some("process"));
    let puts = fake.requests_to("PUT", "/v1/node/allowed-models");
    assert_eq!(puts, vec![json!({"models": ["mock-model"]})]);
}

#[test]
fn chat_sends_model_marker_label_and_a_fresh_thread() {
    let fake = Fake::new();
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!("when:\n  - chat: \"e2e-step:hi Say hi\"\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{}", r.observed);
    let posts = fake.requests_to("POST", "/v1/runs");
    let spec = &posts[0]["spec"];
    assert_eq!(spec["inputs"]["model"], "mock-model");
    assert_eq!(
        spec["inputs"]["prompt"],
        "e2e-step:hi Say hi mock-script:fixture"
    );
    assert!(spec["agentRef"]["resourceId"]["urn"]
        .as_str()
        .unwrap()
        .contains("agent-plain-fixture-tg1"));
    assert!(spec["workspaceRefUri"]
        .as_str()
        .unwrap()
        .starts_with("volume://agent-plain-fixture-tg1"));
}

#[test]
fn world_names_are_run_unique_and_cleanup_cancels_runs() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_RUNNING"],
        ..Default::default()
    });
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: {{ text: \"e2e-step:a go\", budget: 300ms }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.0".to_string(), Class::Harness));
    let puts = fake.requests_to("PUT", "/v1/agents");
    assert!(puts[0].to_string().contains("agent-plain-fixture-tg1"));
    assert_eq!(fake.count("POST", "/v1/agents/"), 1, "released once");
    assert_eq!(
        fake.count("POST", "/v1/runs/"),
        1,
        "the open run is cancelled in cleanup"
    );
    assert_eq!(
        fake.count("POST", "/v1/sessions/"),
        1,
        "its session is terminated"
    );
}

#[test]
fn thread_continue_reuses_the_thread_and_new_does_not() {
    let fake = Fake::new();
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a one\"\n  - chat: {{ text: \"e2e-step:b two\", thread: continue }}\n  - chat: \"e2e-step:c three\"\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{}", r.observed);
    let t: Vec<String> = fake
        .requests_to("POST", "/v1/runs")
        .iter()
        .map(|b| {
            // A follow-up names the thread by id; a new thread sends only its label.
            b["spec"]["threadId"]
                .as_str()
                .or_else(|| b["spec"]["threadRef"].as_str())
                .unwrap()
                .to_string()
        })
        .collect();
    let posts = fake.requests_to("POST", "/v1/runs");
    assert!(posts[0]["spec"]["threadId"].is_null());
    assert_eq!(posts[1]["spec"]["threadId"], "th_1");
    assert_eq!(posts[1]["spec"]["spaceId"], "sp_1");
    assert_ne!(
        t[0], t[1],
        "the first chat sent a label, the follow-up the id"
    );
    assert!(posts[2]["spec"]["threadId"].is_null());
    assert_ne!(posts[2]["spec"]["threadRef"], posts[0]["spec"]["threadRef"]);
    let ws: Vec<String> = fake
        .requests_to("POST", "/v1/runs")
        .iter()
        .map(|b| b["spec"]["workspaceRefUri"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ws[0], ws[1]);
}

#[test]
fn continue_without_an_earlier_chat_is_a_scenario_error() {
    let fake = Fake::new();
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: {{ text: \"e2e-step:a x\", thread: continue }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.0".to_string(), Class::Scenario));
}

#[test]
fn approval_parks_then_resumes() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_RUNNING", "RUN_STATE_PAUSED"],
        after_decide: vec!["RUN_STATE_RUNNING", "RUN_STATE_SUCCEEDED"],
        approval: Some(("APPROVAL_KIND_TOOL_CALL", "write_file")),
        events: vec![("vam.v1.tool.completed", json!({"tool_name": "write_file"}))],
        ..Default::default()
    });
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:ask w\"\n  - expect: {{ approval: pending, tool: write_file }}\n  - approve: latest\nthen:\n  - expect: {{ run: succeeded }}\n  - expect: {{ event: tool.completed, tool: write_file }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let d = fake.requests_to("POST", "/v1/approvals/");
    assert_eq!(d[0]["decision"], "HITL_DECISION_APPROVED");
}

#[test]
fn gate_decision_carries_the_option() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_RUNNING", "RUN_STATE_PAUSED"],
        after_decide: vec!["RUN_STATE_SUCCEEDED"],
        approval: Some(("APPROVAL_KIND_GATE", "")),
        steps: vec![json!({"stepId": "confirm", "state": "STEP_STATE_PAUSED"})],
        ..Default::default()
    });
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen_given("[org-admin, agent-plain, playbook-with-gate]", &format!(
        "when:\n  - start-playbook: {{ playbook: playbook-with-gate, label: run }}\n  - expect: {{ step: confirm, state: waiting }}\n  - decide-gate: {{ option: go }}\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let d = fake.requests_to("POST", "/v1/approvals/");
    assert_eq!(d[0]["decision"], "HITL_DECISION_SUBMITTED");
    assert_eq!(d[0]["response"], json!({"option": "go"}));
    let run = &fake.requests_to("POST", "/v1/runs")[0]["spec"];
    assert!(run["playbookRef"]["resourceId"]["urn"]
        .as_str()
        .unwrap()
        .contains("playbook-with-gate"));
    assert!(run["inputs"]["prompt"]
        .as_str()
        .unwrap()
        .starts_with("e2e-step:run "));
}

#[test]
fn start_playbook_refusal_is_expectable() {
    let fake = Fake::new();
    *fake.refuse_runs.lock().unwrap() = Some("missing required input: topic".into());
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen_given(
        "[org-admin, agent-plain, playbook-one-step]",
        &format!("when:\n  - start-playbook: {{ playbook: playbook-one-step }}\n  - expect: {{ refused: true }}\n{ACTIVE}"),
    );
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    // Without a refusal the same expectation fails.
    *fake.refuse_runs.lock().unwrap() = None;
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.1".to_string(), Class::Product));
}

#[test]
fn stop_session_terminates_the_latest_session() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_RUNNING", "RUN_STATE_PAUSED"],
        ..Default::default()
    });
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\n  - stop-session: {{}}\nthen:\n  - expect: {{ run: cancelled }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    assert!(fake.count("POST", "/v1/sessions/sess-1/terminate") >= 1);
}

#[test]
fn chat_without_wait_returns_while_the_run_is_running_and_can_be_stopped() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_RUNNING"],
        ..Default::default()
    });
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!("when:\n  - chat: {{ text: \"e2e-step:a go\", wait: false }}\n  - stop-session: {{}}\nthen:\n  - expect: {{ run: cancelled }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    assert!(fake.count("POST", "/v1/sessions/sess-1/terminate") >= 1);
    // The same chat with the default wait never sees a terminal or parked run.
    let w = scen(&format!(
        "when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ run: cancelled }}\n{ACTIVE}"
    ));
    let r2 = go(&mut e, &w);
    assert_ne!(r2.report, Report::Pass);
}

#[test]
fn restart_vamd_waits_for_health_and_keeps_state() {
    let fake = Fake::new();
    let mut v = FakeVamd::new(fake.clone());
    v.restart_delay = Duration::from_millis(400);
    v.start().unwrap();
    *fake.guest_mode.lock().unwrap() = Some("process".into());
    let api = Api::new(&v.base_url(), "key", "tg1");
    let mut e = Engine::new(api, Box::new(v), fast(), pieces());
    e.prepare();
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\n  - restart-vamd: {{}}\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"));
    let t = Instant::now();
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    assert!(
        t.elapsed() >= Duration::from_millis(400),
        "waited for health"
    );
    assert_eq!(
        fake.runs.lock().unwrap().len(),
        1,
        "state survived the restart"
    );
}

#[test]
fn a_vamd_that_does_not_come_back_is_env() {
    let fake = Fake::new();
    let mut v = FakeVamd::new(fake.clone());
    v.fail_restart = true;
    v.start().unwrap();
    *fake.guest_mode.lock().unwrap() = Some("process".into());
    let api = Api::new(&v.base_url(), "key", "tg1");
    let mut e = Engine::new(api, Box::new(v), fast(), pieces());
    e.prepare();
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a go\"\n  - restart-vamd: {{}}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.1".to_string(), Class::Env));
}

#[test]
fn vamd_dying_mid_wait_is_env() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_RUNNING"],
        ..Default::default()
    });
    let mut e = make(&fake, fast(), Some("process"));
    e.vamd.stop();
    let s = scen(&format!(
        "when:\n  - chat: {{ text: \"e2e-step:a go\", budget: 300ms }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r).1, Class::Env);
}

#[test]
fn wait_ends_on_the_expected_state() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec![
            "RUN_STATE_RUNNING",
            "RUN_STATE_RUNNING",
            "RUN_STATE_RUNNING",
            "RUN_STATE_SUCCEEDED",
        ],
        ..Default::default()
    });
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"
    ));
    assert_eq!(go(&mut e, &s).report, Report::Pass);
}

#[test]
fn wait_ends_at_once_when_the_run_fails_instead() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_RUNNING", "RUN_STATE_FAILED"],
        ..Default::default()
    });
    let mut cfg = fast();
    cfg.step_budget = Duration::from_secs(20);
    let mut e = make(&fake, cfg, Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"
    ));
    let t = Instant::now();
    let r = go(&mut e, &s);
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "did not wait for the budget"
    );
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
    assert!(r.observed.contains("RUN_STATE_FAILED"), "{}", r.observed);
}

#[test]
fn late_events_after_a_terminal_run_still_count() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_SUCCEEDED"],
        events: vec![("vam.v1.tool.completed", json!({"tool_name": "write_file"}))],
        events_after_polls: 6,
        ..Default::default()
    });
    let mut cfg = fast();
    cfg.settle = Duration::from_secs(2);
    let mut e = make(&fake, cfg, Some("process"));
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ event: tool.completed, tool: write_file }}\n{ACTIVE}"));
    assert_eq!(go(&mut e, &s).report, Report::Pass);
}

#[test]
fn an_event_that_never_comes_fails_after_the_settle_grace() {
    let fake = Fake::new();
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ event: tool.failed }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
}

#[test]
fn step_budget_overrun_is_harness() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_RUNNING"],
        ..Default::default()
    });
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: {{ text: \"e2e-step:a go\", budget: 300ms }}\n{ACTIVE}"
    ));
    let t = Instant::now();
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.0".to_string(), Class::Harness));
    assert!(t.elapsed() < Duration::from_secs(2));
    assert!(r.observed.contains("step budget"), "{}", r.observed);
}

#[test]
fn scenario_budget_overrun_is_reported() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_RUNNING"],
        ..Default::default()
    });
    let mut cfg = fast();
    cfg.scenario_budget = Duration::from_millis(300);
    cfg.step_budget = Duration::from_secs(20);
    let mut e = make(&fake, cfg, Some("process"));
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.0".to_string(), Class::Harness));
    assert!(r.observed.contains("scenario budget"), "{}", r.observed);
}

#[test]
fn an_unmatched_mock_rule_makes_it_a_scenario_failure() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_RUNNING"],
        events: vec![(
            "vam.v1.mock.unmatched",
            json!({"script": "fixture", "reason": "no_rule"}),
        )],
        ..Default::default()
    });
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: {{ text: \"e2e-step:a go\", budget: 300ms }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.0".to_string(), Class::Scenario));
    assert!(r.observed.contains("matched no rule"));
}

#[test]
fn no_guest_mode_fails_process_scenarios_at_when_0_as_env() {
    let fake = Fake::new();
    let mut e = make(&fake, fast(), None);
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.0".to_string(), Class::Env));
    assert_eq!(r.report, Report::Fail);
    assert_eq!(fake.count("POST", "/v1/runs"), 0, "nothing started");
}

#[test]
fn pending_scenarios_are_right_or_wrong_reason() {
    let pending = "status: pending\nexpectFail: { step: when.0, class: env }\n";
    // Right reason: no guest.
    let fake = Fake::new();
    let mut e = make(&fake, fast(), None);
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\n{pending}"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::RedRightReason);
    assert!(!r.report.fails_run());
    // Wrong reason: it passes.
    let fake = Fake::new();
    let mut e = make(&fake, fast(), Some("process"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::RedWrongReason);
    assert!(r.report.fails_run());
}

#[test]
fn file_expectations() {
    let fake = Fake::new();
    for n in 1..=3 {
        fake.files
            .lock()
            .unwrap()
            .insert(format!("sp_{n}/workspace/note.txt"), "private".into());
    }
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ file: {{ path: note.txt, present: true }} }}\n  - expect: {{ file: {{ path: note.txt, equals: private }} }}\n  - expect: {{ file: {{ path: other.txt, absent: true }} }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    // A path already under `workspace/` is not prefixed twice.
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ file: {{ path: workspace/note.txt, present: true }} }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    // A file that should be absent but is there fails.
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ file: {{ path: note.txt, absent: true }} }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
}

#[test]
fn brief_and_summary_have_their_sections() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_FAILED"],
        events: vec![(
            "vam.v1.mock.rule_matched",
            json!({"script": "fixture", "rule": 0}),
        )],
        ..Default::default()
    });
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ run: succeeded }}\nguards:\n  - vam:crates/vam-e2e/src/engine.rs\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let brief = vam_e2e::report::brief(&s, &r, &repo);
    for needle in [
        "# fixture",
        "failed step: `then.0`",
        "class: **product**",
        "Expected vs observed",
        "Last events",
        "Mock trace",
        "vam.v1.mock.rule_matched",
        "vamd log",
        "crates/vam-e2e/src/engine.rs",
    ] {
        assert!(brief.contains(needle), "missing {needle:?} in\n{brief}");
    }
    let sum = vam_e2e::report::summary(
        "tg1",
        "now",
        5,
        json!({}),
        &[(&s, &r, Some("briefs/fixture.md".into()))],
        &[],
        1,
        true,
        &[None],
    );
    assert_eq!(sum["totals"]["fail"], 1);
    assert_eq!(sum["scenarios"][0]["failedStep"], "then.0");
    assert_eq!(sum["scenarios"][0]["class"], "product");
    assert_eq!(sum["scenarios"][0]["report"], "fail");
    assert_eq!(sum["exit"], 1);
}

#[cfg(unix)]
#[test]
fn the_real_process_wrapper_spawns_watches_and_stops() {
    use std::os::unix::fs::PermissionsExt;
    use vam_e2e::vamd::{VamdConfig, VamdProcess};
    let dir = tempfile::tempdir().unwrap();
    let mk = |name: &str, body: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    };
    let cfg = |bin, sub: &str| VamdConfig {
        bin,
        dir: dir.path().join(sub),
        scripts_dir: dir.path().join("scripts"),
        key: "k".into(),
        snapshot_restore: false,
        extra_env: vec![],
        state_dir: None,
        master_key: None,
    };
    // Exits at once: not alive, and its output is in the log tail.
    let mut v = VamdProcess::new(cfg(mk("dies.sh", "echo boom; exit 3"), "a")).unwrap();
    v.start().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(!v.alive());
    assert!(v.log_tail(5).contains("boom"));
    // Stays up until stopped; the environment is cleared except for what the engine sets.
    let mut v = VamdProcess::new(cfg(
        mk(
            "stays.sh",
            "echo \"$VAMD_LISTEN $HOME $SECRET_FROM_CALLER\"; sleep 30",
        ),
        "b",
    ))
    .unwrap();
    std::env::set_var("SECRET_FROM_CALLER", "leak");
    v.start().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(v.alive());
    let tail = v.log_tail(5);
    assert!(
        tail.contains("127.0.0.1:") && !tail.contains("leak"),
        "{tail}"
    );
    v.stop();
    assert!(!v.alive());
    // A missing binary is a start error.
    let mut v = VamdProcess::new(cfg(dir.path().join("nope"), "c")).unwrap();
    assert!(v.start().is_err());
}

#[test]
fn selection_rules() {
    use vam_e2e::run_cmd::{select, RunOpts};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e");
    let cat = Catalog::load(&root);
    let opts = |f: &dyn Fn(&mut RunOpts)| {
        let mut o = RunOpts {
            root: root.clone(),
            scenarios: vec![],
            tags: vec![],
            all: false,
            smoke: false,
            firecracker: false,
            budget: Duration::from_secs(1),
            step_budget: None,
            scenario_budget: None,
            out: None,
            bin_dir: None,
            keep: false,
            state_dir: None,
            bench: None,
        };
        f(&mut o);
        o
    };
    assert!(
        select(&cat, &opts(&|_| {})).is_err(),
        "a selector is required"
    );
    // `--all` takes every scenario; `--smoke` every non-retired process one without the `slow` tag.
    let n = cat.scenarios.len();
    let process = cat
        .scenarios
        .iter()
        .filter(|(_, s)| {
            s.needs == vam_e2e::scenario::Needs::Process
                && s.status != vam_e2e::scenario::Status::Retired
                && !s.tags.iter().any(|t| t == "slow")
        })
        .count();
    assert_eq!(select(&cat, &opts(&|o| o.all = true)).unwrap().len(), n);
    assert_eq!(
        select(&cat, &opts(&|o| o.smoke = true)).unwrap().len(),
        process
    );
    assert_eq!(
        select(
            &cat,
            &opts(&|o| o.scenarios = vec!["plain-chat-gets-a-reply".into()])
        )
        .unwrap()
        .len(),
        1
    );
    // `--firecracker` takes every non-retired `needs: firecracker` scenario (by `needs`, not by
    // name, so later ones join), and never a process one; with `--smoke` it is a usage error.
    let fc: Vec<String> = cat
        .scenarios
        .iter()
        .filter(|(_, s)| {
            s.needs == vam_e2e::scenario::Needs::Firecracker
                && s.status != vam_e2e::scenario::Status::Retired
        })
        .map(|(_, s)| s.id.clone())
        .collect();
    assert!(fc.len() >= 5, "{fc:?}");
    let picked: Vec<String> = select(&cat, &opts(&|o| o.firecracker = true))
        .unwrap()
        .iter()
        .map(|s| s.id.clone())
        .collect();
    assert_eq!(picked, fc);
    assert!(select(
        &cat,
        &opts(&|o| {
            o.firecracker = true;
            o.smoke = true
        })
    )
    .is_err());
    assert!(select(&cat, &opts(&|o| o.scenarios = vec!["nope".into()])).is_err());
    assert!(
        select(&cat, &opts(&|o| o.tags = vec!["smoke".into()])).is_err(),
        "smoke is a flag, not a tag"
    );
    assert!(select(&cat, &opts(&|o| o.tags = vec!["gate".into()]))
        .unwrap()
        .iter()
        .all(|s| s.tags.contains(&"gate".to_string())));
}

#[test]
fn node_expectation_matches_fields_strings_and_bools() {
    let fake = Fake::new();
    *fake.node_extra.lock().unwrap() =
        Some(json!({"isolation": {"summary": "firecracker microVM, jailer on", "jailer": true}}));
    let mut e = make(&fake, fast(), Some("process"));
    let ok = scen(&format!(
        "when:\n  - expect:\n      node:\n        fields:\n          status.guestMode: process\n          status.isolation.jailer: true\n          status.isolation.summary: {{ contains: jailer on }}\n{ACTIVE}"
    ));
    assert_eq!(go(&mut e, &ok).report, Report::Pass);
    for bad in [
        "status.isolation.jailer: false",
        "status.isolation.apiTls: true",
    ] {
        let s = scen(&format!(
            "when:\n  - expect: {{ node: {{ fields: {{ {bad} }} }} }}\n{ACTIVE}"
        ));
        let r = go(&mut e, &s);
        assert_eq!(failed(&r), ("when.0".into(), Class::Product), "{bad}");
    }
}

#[test]
fn plain_http_probe_fails_on_an_api_answer_and_sends_no_key() {
    let fake = Fake::new();
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - expect: {{ plain-http: refused }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.0".into(), Class::Product));
    let bearers = fake.bearers("GET", "/v1/node");
    assert!(
        bearers.last().is_some_and(|b| b.is_empty()),
        "the probe sent a key: {bearers:?}"
    );
}

#[test]
fn plain_http_probe_passes_when_the_connection_is_refused() {
    // A listener that closes every connection at once: no HTTP answer at all.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for c in l.incoming() {
            drop(c)
        }
    });
    let api = Api::new(&format!("http://{addr}"), "key", "tg1");
    assert!(api.probe_plain("/v1/node").is_err());
}

#[test]
fn plain_http_probe_reaches_the_same_address_over_http() {
    // An https base is probed on http:// at the same host and port.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let h = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut c, _) = l.accept().unwrap();
        let mut buf = [0u8; 2048];
        let n = c.read(&mut buf).unwrap();
        let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
        c.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
            .unwrap();
        req
    });
    let api = Api::new(&format!("https://{addr}"), "secret-key", "tg1");
    let r = api.probe_plain("/v1/node").unwrap();
    assert_eq!(r.status, 400);
    let req = h.join().unwrap();
    assert!(req.starts_with("get /v1/node"), "{req}");
    assert!(
        !req.contains("authorization") && !req.contains("secret-key"),
        "{req}"
    );
}

#[test]
fn plain_http_probe_does_not_follow_a_redirect_into_an_api_answer() {
    // The plain port redirects to a second server that answers like the API.
    let api_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let api_addr = api_l.local_addr().unwrap();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for c in api_l.incoming().flatten().take(2) {
            let mut c = c;
            let mut buf = [0u8; 2048];
            let _ = c.read(&mut buf);
            let body = r#"{"status":{"guestMode":"process"}}"#;
            let _ = c.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut c, _)) = l.accept() {
            let mut buf = [0u8; 2048];
            let _ = c.read(&mut buf);
            let _ = c.write_all(
                format!("HTTP/1.1 302 Found\r\nlocation: http://{api_addr}/v1/node\r\ncontent-length: 0\r\n\r\n")
                    .as_bytes(),
            );
        }
    });
    let api = Api::new(&format!("https://{addr}"), "key", "tg1");
    let r = api.probe_plain("/v1/node").unwrap();
    assert_eq!(r.status, 302, "the redirect was followed");
    assert!(r.body.is_empty(), "{}", r.body);
}

#[test]
fn a_tls_client_waits_for_the_node_certificate_and_rejects_a_bad_one() {
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("state/tls/ca.pem");
    let api = Api::with_ca_path("https://127.0.0.1:1", "key", "tg1", Some(ca.clone()));
    // Not written yet: a transport error that names the file, and not healthy.
    let e = api.get("/v1/health").unwrap_err();
    assert!(e.0.contains("no node certificate"), "{}", e.0);
    assert!(!api.healthy());
    // A file that is not a certificate is a clear error, not a panic.
    std::fs::create_dir_all(ca.parent().unwrap()).unwrap();
    std::fs::write(&ca, b"not pem").unwrap();
    let e = api.get("/v1/health").unwrap_err();
    assert!(e.0.contains("unreadable node certificate"), "{}", e.0);
}

mod vmm {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use vam_e2e::vmm_probe::{judge, Verdict};

    fn own() -> (String, String) {
        ("pid:[1]".into(), "mnt:[2]".into())
    }

    fn proc_entry(
        root: &Path,
        pid: &str,
        comm: &str,
        jail: &str,
        uid: &str,
        pidns: &str,
        mntns: &str,
    ) {
        let d = root.join(pid);
        fs::create_dir_all(d.join("ns")).unwrap();
        fs::write(d.join("comm"), format!("{comm}\n")).unwrap();
        fs::write(
            d.join("status"),
            format!("Name:\t{comm}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
        symlink(jail, d.join("root")).unwrap();
        symlink(pidns, d.join("ns/pid")).unwrap();
        symlink(mntns, d.join("ns/mnt")).unwrap();
    }

    const JAIL: &str = "/srv/jailer/firecracker/vm1/root";

    #[test]
    fn a_jailed_process_passes() {
        let t = tempfile::TempDir::new().unwrap();
        proc_entry(
            t.path(),
            "100",
            "firecracker",
            JAIL,
            "1000",
            "pid:[9]",
            "mnt:[8]",
        );
        proc_entry(t.path(), "101", "bash", "/", "0", "pid:[1]", "mnt:[2]");
        fs::create_dir_all(t.path().join("self")).unwrap();
        assert_eq!(judge(t.path(), "/srv/jailer", &own()), Verdict::Jailed(1));
    }

    fn set_seccomp(root: &Path, pid: &str, value: &str) {
        let d = root.join(pid);
        let st = fs::read_to_string(d.join("status")).unwrap();
        fs::write(d.join("status"), format!("{st}Seccomp:\t{value}\n")).unwrap();
        let task = d.join("task/1");
        fs::create_dir_all(&task).unwrap();
        fs::write(task.join("status"), format!("Seccomp:\t{value}\n")).unwrap();
    }

    #[test]
    fn a_seccomp_filter_is_required_only_when_asked() {
        use vam_e2e::vmm_probe::judge_seccomp;
        let t = tempfile::TempDir::new().unwrap();
        proc_entry(
            t.path(),
            "100",
            "firecracker",
            JAIL,
            "1000",
            "pid:[9]",
            "mnt:[8]",
        );
        set_seccomp(t.path(), "100", "2");
        assert_eq!(
            judge_seccomp(t.path(), "/srv/jailer", &own(), true),
            Verdict::Jailed(1)
        );
    }

    #[test]
    fn a_vmm_without_a_seccomp_filter_fails_only_when_asked() {
        use vam_e2e::vmm_probe::judge_seccomp;
        let t = tempfile::TempDir::new().unwrap();
        proc_entry(
            t.path(),
            "100",
            "firecracker",
            JAIL,
            "1000",
            "pid:[9]",
            "mnt:[8]",
        );
        set_seccomp(t.path(), "100", "0");
        assert_eq!(judge(t.path(), "/srv/jailer", &own()), Verdict::Jailed(1));
        match judge_seccomp(t.path(), "/srv/jailer", &own(), true) {
            Verdict::Unjailed(p) => {
                assert!(p[0].contains("Seccomp is 0"), "{p:?}");
                assert!(p[0].contains("1=0"), "{p:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A real jailed Firecracker pivots into a new mount namespace: `readlink
    /// /proc/<pid>/root` reads `/`, but the directory it opens is the jail's root.
    #[test]
    fn a_jail_root_that_reads_as_slash_is_recognised_by_its_inode() {
        let t = tempfile::TempDir::new().unwrap();
        let under = t.path().join("jailer");
        let root = under.join("firecracker/vm1/root");
        fs::create_dir_all(&root).unwrap();
        let alias = t.path().join("elsewhere");
        symlink(&root, &alias).unwrap();
        let procs = t.path().join("proc");
        fs::create_dir_all(&procs).unwrap();
        // The link text is not under `under`; what it resolves to is the jail dir.
        proc_entry(
            &procs,
            "100",
            "firecracker",
            alias.to_str().unwrap(),
            "1000",
            "pid:[9]",
            "mnt:[8]",
        );
        let under = under.to_str().unwrap();
        assert_eq!(judge(&procs, under, &own()), Verdict::Jailed(1));
        // The same process whose root is some other directory is not jailed.
        let other = t.path().join("other");
        fs::create_dir_all(&other).unwrap();
        let t2 = tempfile::TempDir::new().unwrap();
        proc_entry(
            t2.path(),
            "100",
            "firecracker",
            other.to_str().unwrap(),
            "1000",
            "pid:[9]",
            "mnt:[8]",
        );
        assert!(matches!(
            judge(t2.path(), under, &own()),
            Verdict::Unjailed(_)
        ));
    }

    /// The inode fallback only answers "is the root a jail"; it must not mask the other
    /// properties. A process whose root IS a jail dir (found by inode) still fails on uid 0,
    /// the host pid namespace and the host mnt namespace; and a root of `/` or any directory
    /// outside `under` still fails.
    #[test]
    fn the_inode_fallback_does_not_mask_the_other_checks() {
        let t = tempfile::TempDir::new().unwrap();
        let under = t.path().join("jailer");
        let jail = under.join("firecracker/vm1/root");
        fs::create_dir_all(&jail).unwrap();
        let alias = t.path().join("alias");
        symlink(&jail, &alias).unwrap();
        let under = under.to_str().unwrap();
        let alias = alias.to_str().unwrap();
        for (uid, pidns, mntns, needle) in [
            ("0", "pid:[9]", "mnt:[8]", "uid is 0/0"),
            ("1000", "pid:[1]", "mnt:[8]", "pid namespace"),
            ("1000", "pid:[9]", "mnt:[2]", "mnt namespace"),
        ] {
            let p = tempfile::TempDir::new().unwrap();
            proc_entry(p.path(), "100", "firecracker", alias, uid, pidns, mntns);
            match judge(p.path(), under, &own()) {
                Verdict::Unjailed(l) => assert!(l.iter().any(|x| x.contains(needle)), "{l:?}"),
                v => panic!("{needle}: {v:?}"),
            }
        }
        // Roots that are not a jail under `under`: `/` and a plain directory elsewhere.
        let other = t.path().join("other");
        fs::create_dir_all(&other).unwrap();
        for root in ["/", other.to_str().unwrap()] {
            let p = tempfile::TempDir::new().unwrap();
            proc_entry(
                p.path(),
                "100",
                "firecracker",
                root,
                "1000",
                "pid:[9]",
                "mnt:[8]",
            );
            match judge(p.path(), under, &own()) {
                Verdict::Unjailed(l) => assert!(l.iter().any(|x| x.contains("not under")), "{l:?}"),
                v => panic!("{root}: {v:?}"),
            }
        }
        // And the same good process with the good root passes.
        let p = tempfile::TempDir::new().unwrap();
        proc_entry(
            p.path(),
            "100",
            "firecracker",
            alias,
            "1000",
            "pid:[9]",
            "mnt:[8]",
        );
        assert_eq!(judge(p.path(), under, &own()), Verdict::Jailed(1));
    }

    #[test]
    fn each_broken_property_fails() {
        for (jail, uid, pidns, mntns, needle) in [
            ("/", "1000", "pid:[9]", "mnt:[8]", "not under"),
            (JAIL, "0", "pid:[9]", "mnt:[8]", "uid is 0/0"),
            (JAIL, "1000", "pid:[1]", "mnt:[8]", "pid namespace"),
            (JAIL, "1000", "pid:[9]", "mnt:[2]", "mnt namespace"),
        ] {
            let t = tempfile::TempDir::new().unwrap();
            proc_entry(t.path(), "100", "firecracker", jail, uid, pidns, mntns);
            match judge(t.path(), "/srv/jailer", &own()) {
                Verdict::Unjailed(p) => assert!(p.iter().any(|l| l.contains(needle)), "{p:?}"),
                v => panic!("{needle}: {v:?}"),
            }
        }
    }

    #[test]
    fn no_firecracker_process_and_the_under_prefix() {
        let t = tempfile::TempDir::new().unwrap();
        proc_entry(t.path(), "101", "bash", "/", "0", "pid:[1]", "mnt:[2]");
        assert_eq!(judge(t.path(), "/srv/jailer", &own()), Verdict::NoProcess);
        proc_entry(
            t.path(),
            "100",
            "firecracker",
            "/srv/jailerx/a/root",
            "1000",
            "pid:[9]",
            "mnt:[8]",
        );
        assert!(matches!(
            judge(t.path(), "/srv/jailer", &own()),
            Verdict::Unjailed(_)
        ));
        assert_eq!(judge(t.path(), "/srv/jailerx", &own()), Verdict::Jailed(1));
    }
}

#[allow(dead_code)]
fn _unused(_: Value) {}

#[test]
fn thread_expectations() {
    let body = json!({
        "records": [
            {"seq": 1, "kind": "task_start", "body": {"prompt": "Save the note"}},
            {"seq": 2, "kind": "message", "message": {"role": "user", "parts": [{"type": "text", "text": "e2e-step:first Save the note"}]}},
            {"seq": 3, "kind": "message", "message": {"role": "assistant", "parts": [{"type": "tool_use", "id": "c1", "name": "read_file", "arguments": "{\"path\":\"note.txt\"}"}]}},
            {"seq": 4, "kind": "message", "message": {"role": "tool", "parts": [{"type": "tool_result", "tool_use_id": "c1", "content": [{"type": "text", "text": "kiwi-42"}]}]}},
            {"seq": 5, "kind": "message", "message": {"role": "assistant", "parts": [{"type": "text", "text": "Saved."}]}}
        ],
        "integrity": "ok",
        "source": "head"
    });
    let fake = Fake::new();
    fake.files
        .lock()
        .unwrap()
        .insert("sp_1/threads/messages".into(), body.to_string());
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ thread: {{ role: user, contains: Save the note }} }}\n  - expect: {{ thread: {{ role: tool, contains: kiwi-42 }} }}\n  - expect: {{ thread: {{ role: assistant, contains: \"Saved.\" }} }}\n  - expect: {{ thread: {{ role: assistant, contains: note.txt }} }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    // A message of another role does not match, and a task_start body is not a message.
    let fake2 = Fake::new();
    fake2
        .files
        .lock()
        .unwrap()
        .insert("sp_1/threads/messages".into(), body.to_string());
    let mut e = make(&fake2, fast(), Some("process"));
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ thread: {{ role: user, contains: kiwi-42 }} }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
    assert!(r.observed.contains("5 records"), "{}", r.observed);
}

#[test]
fn a_missing_thread_endpoint_is_a_product_failure_at_the_expect() {
    let fake = Fake::new();
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ thread: {{ role: user, contains: go }} }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
    assert!(r.observed.contains("404"), "{}", r.observed);
}

fn parked(tool: &'static str) -> Plan {
    Plan {
        states: vec!["RUN_STATE_RUNNING", "RUN_STATE_PAUSED"],
        after_decide: vec!["RUN_STATE_RUNNING", "RUN_STATE_SUCCEEDED"],
        approval: Some(("APPROVAL_KIND_TOOL_CALL", tool)),
        ..Default::default()
    }
}

fn run_bodies(fake: &Fake) -> Vec<Value> {
    fake.requests_to("POST", "/v1/runs")
        .into_iter()
        .filter(|b| b.get("spec").is_some())
        .collect()
}

#[test]
fn draft_true_sends_the_draft_version_and_a_plain_chat_none() {
    let fake = Fake::new();
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: {{ text: \"e2e-step:a Hi\", draft: true }}\n  - chat: \"e2e-step:b Hi\"\n  - chat: {{ text: \"e2e-step:c Hi\", draft: false }}\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let runs = run_bodies(&fake);
    assert_eq!(
        runs[0]["spec"]["agentRef"]["version"],
        json!({"major": 1, "prerelease": "draft"})
    );
    assert!(runs[1]["spec"]["agentRef"].get("version").is_none());
    // A draft run has a workspace of its own: vamd binds a workspace to one agent version.
    assert!(runs[0]["spec"]["workspaceRefUri"]
        .as_str()
        .unwrap()
        .ends_with("-draft"));
    assert!(!runs[1]["spec"]["workspaceRefUri"]
        .as_str()
        .unwrap()
        .ends_with("-draft"));
    assert_eq!(
        runs[1]["spec"]["workspaceRefUri"],
        runs[2]["spec"]["workspaceRefUri"]
    );
    // `draft: false` pins the release, so a later draft is not followed.
    assert_eq!(
        runs[2]["spec"]["agentRef"]["version"],
        json!({"major": 1, "minor": 0, "patch": 0})
    );
}

#[test]
fn start_playbook_takes_draft_too() {
    let fake = Fake::new();
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen_given(
        "[org-admin, agent-plain, playbook-one-step]",
        &format!("when:\n  - start-playbook: {{ playbook: playbook-one-step, draft: true }}\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"),
    );
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    assert_eq!(
        run_bodies(&fake)[0]["spec"]["agentRef"]["version"]["prerelease"],
        "draft"
    );
}

#[test]
fn always_sends_remember_paired_with_its_decision() {
    for (verb, remember, decision) in [
        ("approve", "REMEMBER_ALWAYS_ALLOW", "HITL_DECISION_APPROVED"),
        ("deny", "REMEMBER_ALWAYS_DENY", "HITL_DECISION_REJECTED"),
    ] {
        let fake = Fake::new();
        fake.plan(parked("write_file"));
        let mut e = make(&fake, fast(), Some("process"));
        let s = scen(&format!(
            "when:\n  - chat: \"e2e-step:a w\"\n  - {verb}: {{ always: true }}\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"
        ));
        let r = go(&mut e, &s);
        assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
        let d = fake.requests_to("POST", "/v1/approvals/");
        assert_eq!(d[0]["remember"], remember);
        assert_eq!(d[0]["decision"], decision);
    }
    // Without `always`, no `remember` is sent.
    let fake = Fake::new();
    fake.plan(parked("write_file"));
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a w\"\n  - approve: {{ refused: false }}\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    assert!(fake.requests_to("POST", "/v1/approvals/")[0]
        .get("remember")
        .is_none());
}

#[test]
fn refused_passes_on_a_4xx_while_the_approval_stays_pending() {
    let fake = Fake::new();
    let mut p = parked("write_file");
    p.refuse_decide = Some(409);
    fake.plan(p);
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a w\"\n  - approve: {{ always: true, refused: true }}\nthen:\n  - expect: {{ approval: pending, tool: write_file }}\n{ACTIVE}"
    ));
    let t = Instant::now();
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "no wait for the run to leave PAUSED"
    );
}

#[test]
fn refused_fails_when_the_decision_is_accepted_or_the_answer_is_not_4xx() {
    // Accepted.
    let fake = Fake::new();
    fake.plan(parked("write_file"));
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a w\"\n  - approve: {{ refused: true }}\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.1".to_string(), Class::Product));
    assert!(r.expected.contains("refused"), "{}", r.expected);
    // A server error is not a refusal.
    let fake = Fake::new();
    let mut p = parked("write_file");
    p.refuse_decide = Some(500);
    fake.plan(p);
    let mut e = make(&fake, fast(), Some("process"));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.1".to_string(), Class::Product));
    // Without `refused`, a 4xx fails the step as before.
    let fake = Fake::new();
    let mut p = parked("write_file");
    p.refuse_decide = Some(409);
    fake.plan(p);
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a w\"\n  - approve: latest\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.1".to_string(), Class::Product));
    assert!(r.expected.contains("accepted"), "{}", r.expected);
}

#[test]
fn decide_gate_refused_has_the_same_rules() {
    let gate_plan = || Plan {
        states: vec!["RUN_STATE_RUNNING", "RUN_STATE_PAUSED"],
        after_decide: vec!["RUN_STATE_SUCCEEDED"],
        approval: Some(("APPROVAL_KIND_GATE", "")),
        steps: vec![json!({"stepId": "confirm", "state": "STEP_STATE_PAUSED"})],
        refuse_decide: Some(400),
        ..Default::default()
    };
    let fake = Fake::new();
    fake.plan(gate_plan());
    let mut e = make(&fake, fast(), Some("process"));
    let given = "[org-admin, agent-plain, playbook-with-gate]";
    let s = scen_given(given, &format!(
        "when:\n  - start-playbook: {{ playbook: playbook-with-gate, label: run }}\n  - decide-gate: {{ option: later, refused: true }}\nthen:\n  - expect: {{ step: confirm, state: waiting }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    assert_eq!(
        fake.requests_to("POST", "/v1/approvals/")[0]["response"],
        json!({"option": "later"})
    );
    // Accepted although refusal was expected.
    let fake = Fake::new();
    let mut p = gate_plan();
    p.refuse_decide = None;
    fake.plan(p);
    let mut e = make(&fake, fast(), Some("process"));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.1".to_string(), Class::Product));
}

#[test]
fn approval_fields_match_claims_by_exact_path() {
    let claims = json!({
        "alwaysOffered": true,
        "target": "builtin:write_file",
        "lockedReason": "released version: approvals are once only",
    });
    let mk = |fields: &str| {
        let fake = Fake::new();
        let mut p = parked("write_file");
        p.claims = claims.clone();
        fake.plan(p);
        let mut e = make(&fake, fast(), Some("process"));
        let s = scen(&format!(
            "when:\n  - chat: \"e2e-step:a w\"\n  - expect: {{ approval: pending, tool: write_file, fields: {fields} }}\n{ACTIVE}"
        ));
        go(&mut e, &s)
    };
    let r = mk("{ alwaysOffered: true, target: \"builtin:write_file\" }");
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let r = mk("{ lockedReason: { contains: once only } }");
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    // A wrong value, a wrong key spelling and a missing key all miss, and say what was pending.
    for bad in [
        "{ alwaysOffered: false }",
        "{ always_offered: true }",
        "{ nothing: 1 }",
    ] {
        let r = mk(bad);
        assert_eq!(failed(&r), ("when.1".to_string(), Class::Product), "{bad}");
        assert!(r.observed.contains("last pending"), "{}", r.observed);
        assert!(r.observed.contains("builtin:write_file"), "{}", r.observed);
    }
}

#[test]
fn gate_expectations_read_the_gate_claims_and_pick_a_step() {
    let gate_plan = || Plan {
        states: vec!["RUN_STATE_RUNNING", "RUN_STATE_PAUSED"],
        approval: Some(("APPROVAL_KIND_GATE", "")),
        claims: json!({"approvalMessage": "Go on?", "requestedArgs": {"options": ["go", "stop"]}}),
        ..Default::default()
    };
    let given = "[org-admin, agent-plain, playbook-with-gate]";
    let mk = |expect: &str| {
        let fake = Fake::new();
        fake.plan(gate_plan());
        let mut e = make(&fake, fast(), Some("process"));
        let s = scen_given(given, &format!(
            "when:\n  - start-playbook: {{ playbook: playbook-with-gate, label: run }}\n  - expect: {expect}\n{ACTIVE}"
        ));
        go(&mut e, &s)
    };
    let r = mk("{ gate: pending, step: confirm, fields: { approvalMessage: \"Go on?\", requestedArgs.options.1: stop } }");
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let r = mk("{ gate: pending }");
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let r = mk("{ gate: pending, step: other }");
    assert_eq!(failed(&r), ("when.1".to_string(), Class::Product));
    let r = mk("{ gate: pending, fields: { requestedArgs.options.2: x } }");
    assert_eq!(failed(&r), ("when.1".to_string(), Class::Product));
    // A gate is not a tool-call approval: there is no tool-call approval to find.
    let r = mk("{ approval: pending }");
    assert_eq!(failed(&r), ("when.1".to_string(), Class::Harness));
}

#[test]
fn a_member_identity_is_created_and_its_key_runs_and_decides() {
    let fake = Fake::new();
    fake.plan(parked("write_file"));
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen_given(
        "[org-admin, org-member, agent-with-guarded-tool]",
        &format!("when:\n  - chat: {{ text: \"e2e-step:a w\", as: org-member }}\n  - expect: {{ approval: pending, tool: write_file }}\n  - approve: latest\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"),
    );
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let users = fake.requests_to("POST", "/v1/users");
    let user = users.iter().find(|u| u.get("username").is_some()).unwrap();
    assert_eq!(user["role"], "member");
    let name = user["username"].as_str().unwrap();
    assert_eq!(name, "org-member-fixture-tg1");
    let password = user["password"].as_str().unwrap().to_string();
    assert_eq!(password.len(), 48, "24 random bytes as hex");
    assert_eq!(fake.count("POST", &format!("/v1/users/{name}/keys")), 1);
    let secret = format!("secret-of-{name}");
    // The run, its approval list, its run reads and its decision all used the member's key;
    // setup used the bootstrap key.
    assert_eq!(
        fake.bearers("POST", "/v1/runs"),
        std::slice::from_ref(&secret)
    );
    assert!(fake
        .bearers("GET", "/v1/approvals")
        .iter()
        .all(|b| *b == secret));
    assert_eq!(
        fake.bearers("POST", "/v1/approvals/"),
        std::slice::from_ref(&secret)
    );
    assert!(fake
        .bearers("GET", "/v1/runs/")
        .iter()
        .all(|b| *b == secret));
    assert!(fake.bearers("POST", "/v1/users").iter().all(|b| b == "key"));
    // Its workspace is its own.
    let ws = run_bodies(&fake)[0]["spec"]["workspaceRefUri"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(ws.ends_with("-org-member"), "{ws}");
    // Neither the password nor the key reaches the brief or the debug output.
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let brief = vam_e2e::report::brief(&s, &r, &repo);
    assert!(
        !brief.contains(&password) && !brief.contains(&secret),
        "{brief}"
    );
    assert!(!format!("{:?}", e.runs()).contains(&secret));
}

#[test]
fn the_default_identity_stays_the_bootstrap_key() {
    let fake = Fake::new();
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen_given(
        "[org-admin, org-member, agent-plain]",
        &format!("when:\n  - chat: \"e2e-step:a Hi\"\n  - chat: {{ text: \"e2e-step:b Hi\", as: org-admin }}\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"),
    );
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    assert_eq!(fake.bearers("POST", "/v1/runs"), ["key", "key"]);
    assert!(!run_bodies(&fake)[1]["spec"]["workspaceRefUri"]
        .as_str()
        .unwrap()
        .contains("org-admin"));
    // Approvals are listed per identity: the admin does not see the member's, so deciding as
    // the admin finds nothing to decide.
    let fake = Fake::new();
    fake.plan(parked("write_file"));
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen_given(
        "[org-admin, org-member, agent-plain]",
        &format!("when:\n  - chat: {{ text: \"e2e-step:a w\", as: org-member }}\n  - approve: {{ as: org-admin }}\n{ACTIVE}"),
    );
    let r = go(&mut e, &s);
    assert_eq!(failed(&r).0, "when.1");
}

#[test]
fn a_run_parked_on_an_approval_fails_a_run_wait_fast_as_product() {
    let fake = Fake::new();
    fake.plan(parked("write_file"));
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a w\"\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"
    ));
    let t = Instant::now();
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
    assert!(
        r.observed.contains("parked on approval for write_file"),
        "{}",
        r.observed
    );
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t.elapsed()
    );
    // Waiting for `paused` itself is not affected, and neither is an approval being decided.
    let fake = Fake::new();
    fake.plan(parked("write_file"));
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a w\"\n  - expect: {{ run: paused }}\n  - approve: latest\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
}

#[test]
fn a_paused_run_without_an_approval_still_runs_out_the_budget_as_harness() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_RUNNING", "RUN_STATE_PAUSED"],
        ..Default::default()
    });
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!(
        "when:\n  - chat: \"e2e-step:a w\"\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"
    ));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Harness));
}

fn scen_fc(extra: &str) -> Scenario {
    let text = format!("id: fixture\ntitle: t\ntags: [chat]\nneeds: firecracker\ngiven: [org-admin, agent-plain]\n{extra}");
    serde_yaml::from_str(&text).unwrap()
}

#[test]
fn firecracker_scenarios_preflight_a_vm_node() {
    let body = format!("when:\n  - chat: \"e2e-step:a go\"\n{ACTIVE}");
    // A vm node runs it.
    let fake = Fake::new();
    let mut e = make(&fake, fast(), Some("vm"));
    let r = go(&mut e, &scen_fc(&body));
    assert_eq!(r.report, Report::Pass, "{}", r.observed);
    // A process node cannot: class env at when.0, nothing started.
    let fake = Fake::new();
    let mut e = make(&fake, fast(), Some("process"));
    let r = go(&mut e, &scen_fc(&body));
    assert_eq!(failed(&r), ("when.0".to_string(), Class::Env));
    assert!(
        r.observed.contains("vm guest not available"),
        "{}",
        r.observed
    );
    assert_eq!(fake.count("POST", "/v1/runs"), 0);
    // And a vm node refuses a process scenario the same way.
    let fake = Fake::new();
    let mut e = make(&fake, fast(), Some("vm"));
    let r = go(&mut e, &scen(&body));
    assert_eq!(failed(&r), ("when.0".to_string(), Class::Env));
}

#[test]
fn a_real_model_is_pinned_allowed_and_sees_no_step_marker() {
    let fake = Fake::new();
    *fake.guest_mode.lock().unwrap() = Some("vm".into());
    let mut v = FakeVamd::new(fake.clone());
    v.start().unwrap();
    let api = Api::new(&v.base_url(), "key", "tg1");
    let mut e = Engine::new(api, Box::new(v), fast(), pieces());
    e.allow_models(&["mock-model".into(), "claude-haiku-5-5".into()]);
    e.set_node_mode("vm");
    e.prepare();
    assert_eq!(
        fake.requests_to("PUT", "/v1/node/allowed-models"),
        vec![json!({"models": ["mock-model", "claude-haiku-5-5"]})]
    );
    e.set_model("claude-haiku-5-5");
    // The scenario needs a process node; the vm override lets it run on this vm node.
    let s = scen(&format!("when:\n  - chat: \"e2e-step:hi Say hi\"\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{}", r.observed);
    let spec = &fake.requests_to("POST", "/v1/runs")[0]["spec"];
    assert_eq!(spec["inputs"]["model"], "claude-haiku-5-5");
    assert_eq!(spec["inputs"]["prompt"], "Say hi");
}

#[test]
fn a_gateway_run_sends_the_model_provider_and_the_mock_never_does() {
    let fake = Fake::new();
    *fake.guest_mode.lock().unwrap() = Some("vm".into());
    let mut v = FakeVamd::new(fake.clone());
    v.start().unwrap();
    let api = Api::new(&v.base_url(), "key", "tg1");
    let mut e = Engine::new(api, Box::new(v), fast(), pieces());
    e.allow_models(&["mock-model".into(), "anthropic/claude-haiku-5.5".into()]);
    e.set_node_mode("vm");
    e.prepare();
    e.set_model_provider("vercel");
    e.set_model("anthropic/claude-haiku-5.5");
    let s = scen(&format!("when:\n  - chat: \"e2e-step:hi Say hi\"\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{}", r.observed);
    e.set_model("mock-model");
    let _ = go(&mut e, &s);
    let runs = fake.requests_to("POST", "/v1/runs");
    assert_eq!(
        runs[0]["spec"]["inputs"]["model"],
        "anthropic/claude-haiku-5.5"
    );
    assert_eq!(runs[0]["spec"]["inputs"]["model_provider"], "vercel");
    assert!(runs[1]["spec"]["inputs"]["model_provider"].is_null());
}

#[test]
fn node_facts_read_the_mode_and_secret_ids_without_a_value() {
    let fake = Fake::new();
    *fake.secrets.lock().unwrap() = vec!["anthropic/api-key".into()];
    let mut e = make(&fake, fast(), Some("vm"));
    let (mode, ids) = e.node_facts().unwrap();
    assert_eq!(mode.as_deref(), Some("vm"));
    assert_eq!(ids, vec!["anthropic/api-key".to_string()]);
    assert_eq!(fake.count("GET", "/v1/secrets"), 1);
    // Only the list is asked for: no per-secret read.
    assert_eq!(fake.count("GET", "/v1/secrets/"), 0);
}

#[test]
fn a_persistent_node_uses_its_state_dir_and_the_given_master_key() {
    use std::os::unix::fs::PermissionsExt;
    use vam_e2e::vamd::{VamdConfig, VamdProcess};
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("vamd.sh");
    std::fs::write(
        &bin,
        "#!/bin/sh\necho \"db=$VAMD_DB_PATH mk=$VAMD_SECRETS_MASTER_KEY\"; sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = |state: Option<std::path::PathBuf>, mk: Option<&str>| VamdConfig {
        bin: bin.clone(),
        dir: dir.path().join("run"),
        scripts_dir: dir.path().join("scripts"),
        key: "k".into(),
        snapshot_restore: false,
        extra_env: vec![],
        state_dir: state,
        master_key: mk.map(String::from),
    };
    let own = dir.path().join("persist");
    let mut v = VamdProcess::new(cfg(Some(own.clone()), Some("cd".repeat(32).as_str()))).unwrap();
    v.start().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let tail = v.log_tail(5);
    v.stop();
    assert!(
        tail.contains(&format!("db={}/vamd.db", own.display())),
        "{tail}"
    );
    assert!(tail.contains(&"cd".repeat(32)), "{tail}");
    // A throwaway node keeps the old behaviour: state under its dir, the test constant.
    let mut v = VamdProcess::new(cfg(None, None)).unwrap();
    v.start().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let tail = v.log_tail(5);
    v.stop();
    assert!(
        tail.contains("run/state/vamd.db") && tail.contains(&"ab".repeat(32)),
        "{tail}"
    );
}

#[test]
fn persistent_node_needs_the_master_key_and_keeps_one_admin_key() {
    use std::os::unix::fs::PermissionsExt;
    use vam_e2e::run_cmd::persistent_node;
    let dir = tempfile::tempdir().unwrap();
    let some = Some(dir.path().join("s"));
    // Throwaway: nothing to do.
    assert!(persistent_node(&None, |_| None).unwrap().is_none());
    // Unset, blank or the test constant: refused, and nothing is created.
    assert!(persistent_node(&some, |_| None)
        .unwrap_err()
        .contains("VAMD_SECRETS_MASTER_KEY"));
    assert!(persistent_node(&some, |_| Some("ab".repeat(32)))
        .unwrap_err()
        .contains("built-in test constant"));
    assert!(!dir.path().join("s").exists());
    // Given: the admin key is made once, private, and reused.
    let own = "ef".repeat(32);
    let (k1, mk) = persistent_node(&some, |_| Some(own.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(mk.as_deref(), Some(own.as_str()));
    let (k2, _) = persistent_node(&some, |_| Some(own.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(k1, k2);
    let mode = std::fs::metadata(dir.path().join("s/node-key"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn step_markers_are_stripped_for_real_models() {
    assert_eq!(
        vam_e2e::engine::strip_step_marker("e2e-step:start Hi there"),
        "Hi there"
    );
    assert_eq!(vam_e2e::engine::strip_step_marker("Hi"), "Hi");
}

fn running_plan() -> Plan {
    Plan {
        states: vec!["RUN_STATE_RUNNING"],
        ..Default::default()
    }
}

#[test]
fn send_posts_a_steer_to_the_runs_thread_without_the_busy_retry() {
    let fake = Fake::new();
    fake.plan(running_plan());
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!("when:\n  - chat: {{ text: \"e2e-step:a go\", wait: false }}\n  - send: {{ text: \"Use the title Final\" }}\nthen:\n  - expect: {{ run: running }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let posts = fake.requests_to("POST", "/v1/runs");
    assert!(posts.len() >= 2);
    assert_eq!(posts[1]["spec"]["inboxMode"], "STEER");
    assert_eq!(posts[1]["spec"]["threadId"], "th_1");
    assert_eq!(posts[1]["spec"]["spaceId"], "sp_1");
    assert_eq!(posts[1]["spec"]["inputs"]["prompt"], "Use the title Final");
}

#[test]
fn send_refused_needs_a_4xx_and_an_accepted_message_fails_it() {
    let fake = Fake::new();
    fake.plan(running_plan());
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!("when:\n  - chat: {{ text: \"e2e-step:a go\", wait: false }}\n  - send: {{ text: \"hi\", refused: true }}\nthen:\n  - expect: {{ run: running }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.1".to_string(), Class::Product));
}

#[test]
fn queued_send_follows_the_run_it_starts() {
    let fake = Fake::new();
    fake.plan(running_plan());
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!("when:\n  - chat: {{ text: \"e2e-step:a go\", wait: false }}\n  - send: {{ text: \"e2e-step:b more\", mode: queue }}\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let posts = fake.requests_to("POST", "/v1/runs");
    assert_eq!(posts[1]["spec"]["inboxMode"], "QUEUE");
    assert!(posts[1]["spec"]["inputs"]["prompt"]
        .as_str()
        .unwrap()
        .contains("mock-script:fixture"));
}

#[test]
fn stop_calls_the_run_stop_route_and_waits_for_the_end() {
    let fake = Fake::new();
    fake.plan(running_plan());
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!("when:\n  - chat: {{ text: \"e2e-step:a go\", wait: false }}\n  - stop: {{}}\nthen:\n  - expect: {{ run: failed }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    assert_eq!(
        fake.count(
            "POST",
            "/v1/runs/urn%3Avam%3Arun%3Alocal.vam.dev%2Facme%2Frun-1/stop"
        ),
        1
    );
}

#[test]
fn a_stop_the_node_does_not_have_is_a_product_failure() {
    let fake = Fake::new();
    fake.plan(running_plan());
    let mut e = make(&fake, fast(), Some("process"));
    // `refused: true` on a stop that is accepted fails the step.
    let s = scen(&format!("when:\n  - chat: {{ text: \"e2e-step:a go\", wait: false }}\n  - stop: {{ refused: true }}\nthen:\n  - expect: {{ run: failed }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r), ("when.1".to_string(), Class::Product));
}

fn question_plan(refuse: Option<u16>) -> Plan {
    Plan {
        states: vec!["RUN_STATE_PAUSED"],
        after_decide: vec!["RUN_STATE_SUCCEEDED"],
        approval: Some(("APPROVAL_KIND_QUESTION", "ask_user")),
        refuse_decide: refuse,
        ..Default::default()
    }
}

#[test]
fn answer_decides_the_pending_question_with_its_response() {
    let fake = Fake::new();
    fake.plan(question_plan(None));
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\n  - answer: {{ response: {{ answer: Ada }} }}\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let d = fake.requests_to("POST", "/v1/approvals/");
    assert_eq!(d[0]["decision"], "HITL_DECISION_SUBMITTED");
    assert_eq!(d[0]["response"]["answer"], "Ada");
}

#[test]
fn a_refused_answer_passes_when_the_question_stays_pending() {
    let fake = Fake::new();
    fake.plan(question_plan(Some(422)));
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\n  - answer: {{ response: {{ answer: 42 }}, refused: true }}\nthen:\n  - expect: {{ approval: pending, tool: ask_user }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
}

#[test]
fn answer_without_a_question_is_a_failure_at_that_step() {
    let fake = Fake::new();
    fake.plan(Plan {
        states: vec!["RUN_STATE_PAUSED"],
        approval: Some(("APPROVAL_KIND_TOOL_CALL", "write_file")),
        ..Default::default()
    });
    let mut e = make(&fake, fast(), Some("process"));
    let s = scen(&format!("when:\n  - chat: \"e2e-step:a go\"\n  - answer: {{ response: {{ answer: Ada }} }}\nthen:\n  - expect: {{ run: succeeded }}\n{ACTIVE}"));
    let r = go(&mut e, &s);
    assert_eq!(failed(&r).0, "when.1");
}

// ---- outcomes per agent version (outcomes.rs) ----------------------------------------------

const OUTCOMES_ACTIVE: &str = "status: active\n";

fn run_with(fake: &std::sync::Arc<Fake>, extra: &str) -> ScenarioResult {
    let mut e = make(fake, fast(), Some("process"));
    go(&mut e, &scen(extra))
}

#[test]
fn agent_stats_reads_the_outcomes_route_and_compares_numbers_by_value() {
    let fake = Fake::new();
    fake.files.lock().unwrap().insert(
        "outcomes".into(),
        json!({"runs": 2, "successes": 2, "passK": {"k": 2, "value": 1.0},
               "effects": {"duplicates": 0}, "costMissing": ["mock-model"]})
        .to_string(),
    );
    let s = format!(
        "when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect:\n      agent-stats:\n        piece: agent-plain\n        version: 1.0.0\n        k: 2\n        fields:\n          runs: 2\n          passK.value: 1\n          passK.k: 2\n          effects.duplicates: 0\n          costMissing.0: mock-model\n          costPerSuccessMicroUsd: {{ absent: true }}\n          tokensPerSuccess: {{ absent: true }}\n{OUTCOMES_ACTIVE}"
    );
    let r = run_with(&fake, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let reqs = fake.requests.lock().unwrap();
    let hit = reqs
        .iter()
        .find(|(m, p, _)| m == "GET" && p.contains("/outcomes"))
        .expect("outcomes read");
    assert!(hit.1.ends_with("/outcomes?version=1.0.0&k=2"), "{}", hit.1);
}

#[test]
fn agent_stats_names_the_field_that_differs_and_a_404_is_product() {
    let fake = Fake::new();
    fake.files
        .lock()
        .unwrap()
        .insert("outcomes".into(), json!({"runs": 3}).to_string());
    let s = format!(
        "when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ agent-stats: {{ piece: agent-plain, fields: {{ runs: 2 }} }} }}\n{OUTCOMES_ACTIVE}"
    );
    let r = run_with(&fake, &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
    assert!(r.observed.contains("runs: 3 (want 2)"), "{}", r.observed);
    // No route at all: the 404 is a product failure at that step.
    let r = run_with(&Fake::new(), &s);
    assert_eq!(failed(&r), ("then.0".to_string(), Class::Product));
    assert!(r.observed.contains("answered 404"), "{}", r.observed);
}

#[test]
fn release_diff_reads_the_diff_route_for_a_version() {
    let fake = Fake::new();
    fake.files.lock().unwrap().insert(
        "outcomes-diff".into(),
        json!({"base": {"version": "1.0.0", "runs": 1}, "candidate": {"version": "draft", "runs": 1}})
            .to_string(),
    );
    let s = format!(
        "when:\n  - chat: \"e2e-step:a go\"\nthen:\n  - expect: {{ release-diff: {{ piece: agent-plain, version: draft, fields: {{ base.version: 1.0.0, candidate.runs: 1 }} }} }}\n{OUTCOMES_ACTIVE}"
    );
    let r = run_with(&fake, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let reqs = fake.requests.lock().unwrap();
    assert!(reqs
        .iter()
        .any(|(m, p, _)| m == "GET" && p.ends_with("/outcomes/diff?version=draft")));
}

#[test]
fn edit_agent_draft_reads_the_draft_sets_the_path_and_puts_it_back() {
    let fake = Fake::new();
    let s = format!(
        "when:\n  - chat: \"e2e-step:a go\"\n  - edit-agent-draft: {{ piece: agent-plain, set: {{ description: second edition, permissions.tools.default: allow }} }}\n{OUTCOMES_ACTIVE}"
    );
    let r = run_with(&fake, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let puts = fake.requests_to("PUT", "/v1/agents");
    // The piece's own put, then the edit.
    assert_eq!(puts.len(), 2);
    let edit = puts.last().unwrap();
    assert_eq!(edit["spec"]["description"], "second edition");
    assert_eq!(edit["spec"]["permissions"]["tools"]["default"], "allow");
    assert!(edit["spec"]["resourceId"]["urn"]
        .as_str()
        .unwrap()
        .starts_with("urn:vam:agent:"));
}

#[test]
fn a_draft_edit_through_a_scalar_is_a_scenario_error() {
    let fake = Fake::new();
    let s = format!(
        "when:\n  - edit-agent-draft: {{ piece: agent-plain, set: {{ description.x: 1 }} }}\n{OUTCOMES_ACTIVE}"
    );
    let r = run_with(&fake, &s);
    assert_eq!(failed(&r), ("when.0".to_string(), Class::Scenario));
}

#[test]
fn release_agent_posts_the_version_and_refused_wants_a_4xx() {
    let fake = Fake::new();
    let s = format!(
        "when:\n  - release-agent: {{ piece: agent-plain, version: 1.1.0 }}\n{OUTCOMES_ACTIVE}"
    );
    let r = run_with(&fake, &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let posts = fake.requests_to("POST", "/v1/agents/");
    assert_eq!(
        posts.last().unwrap(),
        &json!({"version": {"major": 1, "minor": 1, "patch": 0}})
    );
    // The fake answers 200, so a step that wants a refusal fails as product.
    let s = format!(
        "when:\n  - release-agent: {{ piece: agent-plain, version: 1.2.0, refused: true }}\n{OUTCOMES_ACTIVE}"
    );
    let r = run_with(&Fake::new(), &s);
    assert_eq!(failed(&r), ("when.0".to_string(), Class::Product));
}

/// A stand-in for `vam`: exit 0 when the file it is given is byte-equal to `$ORIG`, else exit 1
/// saying the signature does not match; every call's arguments go to `$LOG`.
fn fake_vam(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("vam");
    std::fs::write(
        &p,
        "#!/bin/sh\necho \"$@\" >> \"$LOG\"\nif cmp -s \"$3\" \"$ORIG\"; then echo verified; exit 0; fi\necho 'signature does not match'; exit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn verify_receipt_file_passes_with_pinned_keys_and_fails_on_a_tampered_copy() {
    let dir = std::env::temp_dir().join(format!("outcomes-verify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let receipt = dir.join("receipt.json");
    std::fs::write(
        &receipt,
        json!({"receipt": {"pins": {"agent": "a"}}, "modelAppendix": {}, "signature": {"keyId": "k"}})
            .to_string(),
    )
    .unwrap();
    let keys = dir.join("keys.json");
    std::fs::write(&keys, "{\"signingKeys\":[]}").unwrap();
    let log = dir.join("log");
    std::env::set_var("VAM_E2E_CLI", fake_vam(&dir));
    std::env::set_var("ORIG", &receipt);
    std::env::set_var("LOG", &log);
    let (rf, kf) = (receipt.display(), keys.display());
    let s = format!(
        "when:\n  - verify-receipt: {{ file: {rf}, trustedKeys: {kf} }}\n  - verify-receipt: {{ file: {rf}, trustedKeys: {kf}, tamper: true }}\n{OUTCOMES_ACTIVE}"
    );
    let r = run_with(&Fake::new(), &s);
    assert_eq!(r.report, Report::Pass, "{} / {}", r.expected, r.observed);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls.lines().count(), 2, "{calls}");
    for l in calls.lines() {
        assert!(l.starts_with("receipt verify "), "{l}");
        assert!(l.ends_with(&format!("--trusted-keys {kf}")), "{l}");
    }
    // A plain verify of a file the stand-in rejects is a product failure naming the output.
    std::fs::write(&receipt, "{}").unwrap();
    std::fs::write(dir.join("other"), "x").unwrap();
    std::env::set_var("ORIG", dir.join("other"));
    let s = format!("when:\n  - verify-receipt: {{ file: {rf} }}\n{OUTCOMES_ACTIVE}");
    let r = run_with(&Fake::new(), &s);
    assert_eq!(failed(&r), ("when.0".to_string(), Class::Product));
    assert!(
        r.observed.contains("signature does not match"),
        "{}",
        r.observed
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn outcomes_steps_are_checked_for_shape() {
    let bad = |text: &str| -> Vec<String> {
        let e: vam_e2e::scenario::Expect = serde_yaml::from_str(text).unwrap();
        let given = ["agent-plain".to_string()].into_iter().collect();
        vam_e2e::engine::outcomes::check_expect(&e, &given)
    };
    assert!(
        bad("{ agent-stats: { piece: agent-plain, version: 1.0.0, fields: { runs: 1 } } }")
            .is_empty()
    );
    assert_eq!(
        bad("{ agent-stats: { piece: nobody, fields: { runs: 1 } } }").len(),
        1
    );
    assert_eq!(
        bad("{ agent-stats: { piece: agent-plain, version: latest, fields: {} } }").len(),
        2
    );
    assert_eq!(
        bad("{ release-diff: { piece: agent-plain, version: v1, k: 0, fields: { a: 1 } } }").len(),
        2
    );
}
