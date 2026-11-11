//! Runs one scenario through the engine and keeps the evidence for its brief.

use crate::engine::{Engine, Fail, RunRec};
use crate::runner::{report, Outcome, Report, StepExecutor, StepOutcome};
use crate::scenario::{Class, Scenario};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub runs: Vec<RunRec>,
    /// Receipt and thread of each run, same order as `runs` (usage, H-0.8).
    pub extras: Vec<crate::usage::RunExtra>,
    pub refusal: Option<String>,
    pub vamd_log: String,
    pub guest_logs: Vec<(PathBuf, String)>,
    /// The world's ledger at the end, `(tool, title, count)` (empty without a counting broker).
    pub ledger: Vec<(String, String, u64)>,
}

#[derive(Debug, Clone)]
pub struct ScenarioResult {
    pub outcome: Outcome,
    pub report: Report,
    pub expected: String,
    pub observed: String,
    pub duration_ms: u128,
    /// Cut off by the step or scenario budget: a timeout, not a verdict on the model.
    pub timed_out: bool,
    pub evidence: Evidence,
}

pub fn run_scenario(e: &mut Engine, s: &Scenario) -> ScenarioResult {
    e.begin();
    let t = Instant::now();
    let mut failure: Option<(String, Fail)> = None;
    match e.arrange_full(s) {
        Err(f) => failure = Some(("given".into(), f)),
        Ok(_) => {
            e.quiesce_notifications();
            for (pos, _) in s.steps() {
                if let StepOutcome::Failed {
                    class,
                    expected,
                    observed,
                } = e.exec(s, &pos)
                {
                    failure = Some((pos, Fail::new(class, expected, observed)));
                    break;
                }
            }
        }
    }
    let timed_out = e.budget_timed_out();
    let extras = e.measure_runs();
    let ledger = e.world_ledger();
    let evidence = Evidence {
        ledger,
        runs: e.runs().to_vec(),
        extras,
        refusal: e.last_refusal().map(String::from),
        vamd_log: e.vamd_log_tail(40),
        guest_logs: e
            .guest_logs()
            .into_iter()
            .map(|p| {
                let tail = crate::vamd::tail_of(&p, 30);
                (p, tail)
            })
            .collect(),
    };
    e.cleanup_full();
    let (outcome, expected, observed) = match failure {
        None => (Outcome::Passed, String::new(), String::new()),
        Some((position, f)) => (
            Outcome::Failed {
                position,
                class: f.class,
            },
            f.expected,
            f.observed,
        ),
    };
    let rep = report(s, &outcome);
    ScenarioResult {
        outcome,
        report: rep,
        expected,
        observed,
        duration_ms: t.elapsed().as_millis(),
        timed_out,
        evidence,
    }
}

impl ScenarioResult {
    /// What the scenario's runs cost and how their tool calls went.
    pub fn measure(&self, catalog: &vam_inference::ModelCatalog) -> crate::usage::Measure {
        let ev = &self.evidence;
        let keys: Vec<String> = ev
            .runs
            .iter()
            .map(|r| {
                if r.workspace_id.is_empty() || r.thread_id.is_empty() {
                    String::new()
                } else {
                    format!("{}/{}", r.workspace_id, r.thread_id)
                }
            })
            .collect();
        let runs: Vec<crate::usage::RunInput> = ev
            .runs
            .iter()
            .enumerate()
            .map(|(i, r)| crate::usage::RunInput {
                events: &r.events,
                error: &r.error,
                receipt: ev.extras.get(i).and_then(|x| x.receipt.as_ref()),
                thread: ev.extras.get(i).and_then(|x| x.thread.as_ref()),
                thread_key: &keys[i],
            })
            .collect();
        let mut m = crate::usage::measure(&runs, self.duration_ms, catalog);
        m.duplicate_effects = crate::usage::duplicate_effects(&ev.ledger);
        m.lost_tasks = ev
            .runs
            .iter()
            .filter(|r| {
                !r.run_id.is_empty() && crate::usage::lost_task(&r.state, &r.error, &r.events)
            })
            .count() as u64;
        m
    }
}

pub fn class_name(c: Class) -> &'static str {
    match c {
        Class::Product => "product",
        Class::Scenario => "scenario",
        Class::Harness => "harness",
        Class::Env => "env",
    }
}

/// The fixed next action for each class (design §7).
pub fn class_action(c: Class) -> &'static str {
    match c {
        Class::Product => "the behaviour is wrong: fix the code",
        Class::Scenario => {
            "the scenario or mock script is out of date: fix the scenario and say so in the PR"
        }
        Class::Harness => "the runner or a world piece is broken: fix the harness",
        Class::Env => "vamd or the machine is not ready: check the log, change no code",
    }
}
