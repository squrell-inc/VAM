//! Seams for the runner engine. Nothing here does vamd I/O.

use crate::scenario::{Class, Scenario};

/// A run tag that makes world-piece names unique to one invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTag(pub String);

/// `<piece>-<scenario>-<runTag>`.
pub fn piece_name(piece: &str, scenario: &str, tag: &RunTag) -> String {
    format!("{piece}-{scenario}-{}", tag.0)
}

/// What a step did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    Ok,
    Failed {
        class: Class,
        expected: String,
        observed: String,
    },
}

/// State a scenario arranged, to clean up afterwards (S3).
#[derive(Debug, Default)]
pub struct Arranged {
    pub names: Vec<String>,
}

/// Arranges world pieces and cleans them up. The engine implements it over the HTTP API.
pub trait World {
    fn arrange(&mut self, scenario: &Scenario, tag: &RunTag) -> Result<Arranged, String>;
    fn cleanup(&mut self, arranged: Arranged) -> Result<(), String>;
}

/// Executes one step of a scenario. `position` is `when.N` / `then.N`.
pub trait StepExecutor {
    fn exec(&mut self, scenario: &Scenario, position: &str) -> StepOutcome;
}

/// Where and why a scenario run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed { position: String, class: Class },
}

/// How a scenario is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    Pass,
    Fail,
    RedRightReason,
    RedWrongReason,
}

impl Report {
    /// Only `Fail` and `RedWrongReason` fail the invocation.
    pub fn fails_run(self) -> bool {
        matches!(self, Report::Fail | Report::RedWrongReason)
    }
}

/// S8: a pending scenario that fails at `expectFail.step` with `expectFail.class` is
/// `red (right reason)`; one that passes, or fails elsewhere, is `red (wrong reason)`.
pub fn report(scenario: &Scenario, outcome: &Outcome) -> Report {
    use crate::scenario::Status;
    match (scenario.status, outcome) {
        (Status::Pending, Outcome::Passed) => Report::RedWrongReason,
        (Status::Pending, Outcome::Failed { position, class }) => match &scenario.expect_fail {
            Some(ef) if ef.step == *position && ef.class == *class => Report::RedRightReason,
            _ => Report::RedWrongReason,
        },
        (_, Outcome::Passed) => Report::Pass,
        (_, Outcome::Failed { .. }) => Report::Fail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> Scenario {
        serde_yaml::from_str(
            "id: x\ntitle: t\ntags: [chat]\nneeds: process\nwhen: [{approve: latest}]\nstatus: pending\nexpectFail: {step: when.0, class: env}\n",
        )
        .unwrap()
    }

    #[test]
    fn names_are_run_unique() {
        assert_eq!(
            piece_name("agent-plain", "s", &RunTag("t1".into())),
            "agent-plain-s-t1"
        );
    }

    #[test]
    fn pending_reporting() {
        let s = pending();
        let at = |p: &str, c| Outcome::Failed {
            position: p.into(),
            class: c,
        };
        assert_eq!(
            report(&s, &at("when.0", Class::Env)),
            Report::RedRightReason
        );
        assert_eq!(
            report(&s, &at("when.1", Class::Env)),
            Report::RedWrongReason
        );
        assert_eq!(
            report(&s, &at("when.0", Class::Product)),
            Report::RedWrongReason
        );
        assert_eq!(report(&s, &Outcome::Passed), Report::RedWrongReason);
        assert!(Report::RedWrongReason.fails_run() && !Report::RedRightReason.fails_run());
    }
}
