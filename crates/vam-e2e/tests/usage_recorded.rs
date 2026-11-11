//! Usage counts on runs recorded from a `--smoke` run (trimmed to the fields the counters read;
//! no keys, no host paths).

use serde_json::Value;
use std::path::Path;
use vam_e2e::usage::{measure, Measure, RunInput};
use vam_inference::ModelCatalog;

fn measured(name: &str) -> Measure {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/measure")
        .join(format!("{name}.json"));
    let runs: Vec<Value> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let inputs: Vec<RunInput> = runs
        .iter()
        .map(|r| RunInput {
            events: r["events"].as_array().unwrap(),
            error: r["error"].as_str().unwrap_or_default(),
            receipt: Some(&r["receipt"]),
            thread: Some(&r["thread"]),
            thread_key: r["key"].as_str().unwrap_or_default(),
        })
        .collect();
    measure(&inputs, 1000, &ModelCatalog::new())
}

/// The scenario sends three chats with bad arguments. The mock's repeat rule matches again
/// because the failed call never completed, so each of those threads holds two typed errors
/// (tool call ids `..._bad_0` and `..._bad_1`): six in all, each in its own thread. The model
/// answers in text after them and never makes a valid call in the same thread, so none is
/// repaired. The fourth chat's valid calls are in another thread.
#[test]
fn invalid_calls_are_counted_per_thread_without_double_counting() {
    let m = measured("invalid-tool-arguments-return-a-typed-error");
    assert_eq!(m.invalid, 6);
    assert_eq!(m.repairs, 0);
    assert_eq!((m.loop_warnings, m.loop_stops), (0, 0));
    assert!(m.usage.estimated && m.usage.cost_missing);
}

/// The status error of this run is empty; the stop is in the `run.completed` payload.
#[test]
fn a_loop_stop_is_read_from_the_run_completed_event() {
    let m = measured("repeated-identical-call-is-caught-and-observed");
    assert_eq!(m.loop_warnings, 1);
    assert_eq!(m.loop_stops, 1);
    assert_eq!(m.invalid, 0);
    assert_eq!(m.harness.as_ref().unwrap()["profile"], "base");
}
