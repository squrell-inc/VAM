//! What `--bench` refuses and what it never publishes. No vamd, no network: replay of the
//! synthetic recorded trials, and refusals that happen before a node starts.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn recorded() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bench")
}

fn catalog_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e")
}

/// Runs `vam-e2e run ...` from `cwd` with the provider keys removed from the environment.
fn run(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vam-e2e"))
        .current_dir(cwd)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .arg("run")
        .args(args)
        .output()
        .expect("vam-e2e runs")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn a_recorded_run_marks_every_row_synthetic_and_never_writes_the_publish_dir() {
    let cwd = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let rec = recorded();
    let o = run(
        cwd.path(),
        &[
            "--bench",
            "--recorded",
            rec.to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let results: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.path().join("bench-results.json")).unwrap())
            .unwrap();
    assert_eq!(results["recorded"], true);
    assert_eq!(results["synthetic"], true);
    assert_eq!(results["publishable"], false);
    for row in results["rows"].as_array().unwrap() {
        assert_eq!(row["recorded"], true, "{row}");
        assert_eq!(row["synthetic"], true, "{row}");
    }
    assert!(
        !cwd.path().join("bench").exists(),
        "a recorded run must not write bench/results"
    );
}

#[test]
fn recorded_numbers_follow_the_fixture() {
    let out = tempfile::tempdir().unwrap();
    let rec = recorded();
    let o = run(
        out.path(),
        &["--bench", "--recorded", rec.to_str().unwrap(), "--out", "o"],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let results: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.path().join("o/bench-results.json")).unwrap())
            .unwrap();
    let rows = results["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4, "two tasks x two models");
    let row = |task: &str, model: &str| {
        rows.iter()
            .find(|r| r["task"] == task && r["model"] == model)
            .unwrap_or_else(|| panic!("no row {task} {model}"))
    };
    let s = row("first-try-tool-use", "claude-sonnet-5-5");
    assert_eq!(s["k"], 3);
    assert_eq!(s["successes"], 2);
    assert_eq!(s["passK"], false);
    assert_eq!(s["passAtK"], true);
    // Spend from events: (3600 + 3300 + 3100 + 3300 + 4800 + 4400) / 2 successes.
    assert_eq!(s["costPerSuccess"], 11250.0);
    assert_eq!(s["costMissing"], false);
    // One invalid call, repaired, per two successes; one loop warning and one stop.
    assert_eq!(s["invalidCalls"], 1);
    assert_eq!(s["repairsPerSuccess"], 0.5);
    assert_eq!(s["loopDetections"], 2);
    // Haiku has no spend on its events: priced from the catalog, so the cost is present.
    let h = row("first-try-tool-use", "claude-haiku-5-5");
    assert!(h["costPerSuccess"].as_f64().unwrap() > 0.0, "{h}");
    assert_eq!(row("match-playbook", "claude-haiku-5-5")["passK"], true);
    assert_eq!(results["harnessProfile"], "base");
    assert_eq!(results["endpoint"], "api.anthropic.com");
}

#[test]
fn a_real_model_is_refused_without_real_and_on_the_process_node() {
    let cwd = tempfile::tempdir().unwrap();
    let root = catalog_root();
    let base = |extra: &[&'static str]| {
        let mut a = vec![
            "--bench",
            "--root",
            root.to_str().unwrap(),
            "--model",
            "claude-haiku-5-5",
        ];
        a.extend_from_slice(extra);
        run(cwd.path(), &a)
    };
    let o = base(&[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("--real"), "{}", stderr(&o));
    let o = Command::new(env!("CARGO_BIN_EXE_vam-e2e"))
        .current_dir(cwd.path())
        .env("ANTHROPIC_API_KEY", "sk-test-not-a-real-key")
        .args([
            "run",
            "--bench",
            "--real",
            "--root",
            root.to_str().unwrap(),
            "--model",
            "claude-haiku-5-5",
        ])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    let e = stderr(&o);
    assert!(e.contains("microVM lane"), "{e}");
    assert!(
        !e.contains("sk-test-not-a-real-key"),
        "the key must never be printed: {e}"
    );
    assert!(!cwd.path().join("bench").exists());
}

#[test]
fn an_unpinned_task_set_version_is_refused_for_a_live_selection_too() {
    let cwd = tempfile::tempdir().unwrap();
    let root = catalog_root();
    let o = run(
        cwd.path(),
        &[
            "--bench",
            "--mock",
            "--root",
            root.to_str().unwrap(),
            "--task-set-version",
            "9.9.9",
        ],
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("task-set"), "{}", stderr(&o));
}

#[test]
fn bench_flags_need_bench() {
    let cwd = tempfile::tempdir().unwrap();
    let o = run(cwd.path(), &["--mock", "--smoke"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("--bench"), "{}", stderr(&o));
}

#[test]
fn a_persistent_state_dir_needs_its_own_master_key() {
    let cwd = tempfile::tempdir().unwrap();
    let root = catalog_root();
    let state = cwd.path().join("state");
    let go = |mk: Option<&str>| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vam-e2e"));
        c.current_dir(cwd.path())
            .env_remove("VAMD_SECRETS_MASTER_KEY");
        if let Some(m) = mk {
            c.env("VAMD_SECRETS_MASTER_KEY", m);
        }
        c.args([
            "run",
            "--bench",
            "--mock",
            "--root",
            root.to_str().unwrap(),
            "--scenarios",
            "greeting-starts-no-playbook",
            "--state-dir",
            state.to_str().unwrap(),
        ])
        .output()
        .unwrap()
    };
    let o = go(None);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("VAMD_SECRETS_MASTER_KEY"),
        "{}",
        stderr(&o)
    );
    let o = go(Some(&"ab".repeat(32)));
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("built-in test constant"),
        "{}",
        stderr(&o)
    );
    assert!(
        !state.exists(),
        "nothing is created before the key is accepted"
    );
    // `vam-e2e node` holds the same rule.
    let o = Command::new(env!("CARGO_BIN_EXE_vam-e2e"))
        .env_remove("VAMD_SECRETS_MASTER_KEY")
        .args(["node", "--state-dir", state.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("VAMD_SECRETS_MASTER_KEY"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn an_unknown_profile_is_refused_before_any_trial() {
    let cwd = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let root = catalog_root();
    let o = run(
        cwd.path(),
        &[
            "--root",
            root.to_str().unwrap(),
            "--bench",
            "--mock",
            "--profile",
            "no-such-profile",
            "--out",
            out.path().to_str().unwrap(),
        ],
    );
    assert_eq!(o.status.code(), Some(2));
    let e = stderr(&o);
    assert!(e.contains("profile") && e.contains("open-small"), "{e}");
    assert!(!out.path().join("bench-results.json").exists());
}

#[test]
fn profile_and_max_usd_need_bench_and_a_positive_cap() {
    let cwd = tempfile::tempdir().unwrap();
    for args in [vec!["--profile", "base"], vec!["--max-usd", "1"]] {
        let o = run(cwd.path(), &args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(stderr(&o).contains("need --bench"), "{}", stderr(&o));
    }
    let o = run(
        cwd.path(),
        &[
            "--root",
            catalog_root().to_str().unwrap(),
            "--bench",
            "--mock",
            "--max-usd",
            "0",
        ],
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("--max-usd"), "{}", stderr(&o));
}
