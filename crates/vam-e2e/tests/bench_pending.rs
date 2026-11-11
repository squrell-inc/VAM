//! The folded bench (`vam-e2e run --bench`), proven on recorded run events and receipts, not a
//! live model.

use std::path::{Path, PathBuf};
use std::process::Command;

fn recorded() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bench")
}

/// Runs the bench over the recorded fixtures and returns (exit ok, stderr, results JSON).
fn bench(extra: &[&str]) -> (bool, String, serde_json::Value) {
    let out = tempfile::tempdir().expect("tempdir");
    let o = Command::new(env!("CARGO_BIN_EXE_vam-e2e"))
        .args(["run", "--bench", "--recorded"])
        .arg(recorded())
        .arg("--out")
        .arg(out.path())
        .args(extra)
        .output()
        .expect("vam-e2e runs");
    let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
    let results = std::fs::read(out.path().join("bench-results.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(serde_json::Value::Null);
    (o.status.success(), stderr, results)
}

#[test]
fn bench_run_records_cost_per_success() {
    let (ok, stderr, results) = bench(&[]);
    assert!(ok, "bench run failed: {stderr}");
    let rows = results["rows"].as_array().expect("results have rows");
    assert!(!rows.is_empty(), "no rows: {results}");
    for row in rows {
        for key in ["task", "model", "mode"] {
            assert!(row[key].is_string(), "row lacks {key}: {row}");
        }
        for key in [
            "passK",
            "turns",
            "tokens",
            "costPerSuccess",
            "repairsPerSuccess",
        ] {
            assert!(!row[key].is_null(), "row lacks {key}: {row}");
        }
    }
}

#[test]
fn bench_task_set_version_is_pinned_in_results() {
    let (ok, stderr, results) = bench(&[]);
    assert!(ok, "bench run failed: {stderr}");
    for key in [
        "taskSetVersion",
        "harnessProfile",
        "harnessOverrides",
        "k",
        "endpoint",
    ] {
        assert!(!results[key].is_null(), "results lack {key}: {results}");
    }
    let (ok, stderr, _) = bench(&["--task-set-version", "0.0.0-not-a-set"]);
    assert!(!ok, "a task-set version not in the set must be refused");
    assert!(
        stderr.contains("task-set"),
        "refusal names the task set: {stderr}"
    );
}
