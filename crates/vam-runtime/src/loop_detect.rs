//! Loop detector (`loop.detect`, docs: runtime/agent).
//!
//! Three patterns over the task's own tool calls: the same call N times in a row,
//! an A-B alternation of M cycles, the same error result R times in a row. The
//! first time a pattern holds at the end of a round the detector asks for one
//! observation; if the pattern still holds at the end of a later round it asks to
//! stop. A call that breaks a pattern clears that pattern's warning. State is in
//! memory only: a resumed task starts clean.

use vam_spec::call_check::{INVALID_ARGS_PREFIX, LOOP_OBSERVATION_PREFIX};
use vam_spec::harness::Loop;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    IdenticalCalls,
    Alternation,
    IdenticalErrors,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::IdenticalCalls => "identical_calls",
            Kind::Alternation => "alternation",
            Kind::IdenticalErrors => "identical_errors",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Continue,
    /// Inject this observation (the text names the pattern and what to do).
    Warn(Kind, String),
    /// Stop the task; the string is the detail for the error.
    Stop(Kind, String),
}

struct Record {
    tool: String,
    key: String,
    error: Option<String>,
}

pub struct Detector {
    identical: usize,
    cycles: usize,
    errors: usize,
    history: Vec<Record>,
    warned: [bool; 3],
}

/// A tool result that is an error: the runtime's own error shapes and typed
/// invalid-argument errors.
pub fn is_error_result(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("Error")
        || t.starts_with("error")
        || t.starts_with("tool_error")
        || t.starts_with(INVALID_ARGS_PREFIX)
}

impl Detector {
    pub fn new(cfg: &Loop) -> Self {
        Self {
            identical: cfg.identical_calls as usize,
            cycles: cfg.cycles as usize,
            errors: cfg.identical_errors as usize,
            history: Vec::new(),
            warned: [false; 3],
        }
    }

    /// Record one executed (or rejected) call: its tool, its canonical arguments
    /// (equal for equal JSON whatever the key order) and its result text.
    pub fn observe(&mut self, tool: &str, canonical_args: &str, result: &str) {
        self.history.push(Record {
            tool: tool.to_string(),
            key: format!("{tool}\u{0}{canonical_args}"),
            error: is_error_result(result).then(|| result.to_string()),
        });
        let keep = 2 * self.identical.max(self.cycles * 2).max(self.errors).max(4);
        if self.history.len() > keep {
            let drop = self.history.len() - keep;
            self.history.drain(..drop);
        }
    }

    /// A round with no tool calls breaks every pattern.
    pub fn clear(&mut self) {
        self.history.clear();
        self.warned = [false; 3];
    }

    fn identical_holds(&self) -> Option<String> {
        let n = self.identical;
        if n == 0 || self.history.len() < n {
            return None;
        }
        let tail = &self.history[self.history.len() - n..];
        tail.iter().all(|r| r.key == tail[0].key).then(|| {
            format!(
                "{} called with identical arguments {n} times in a row",
                tail[0].tool
            )
        })
    }

    fn alternation_holds(&self) -> Option<String> {
        let len = self.cycles * 2;
        if self.cycles == 0 || self.history.len() < len {
            return None;
        }
        let tail = &self.history[self.history.len() - len..];
        let (a, b) = (&tail[0], &tail[1]);
        if a.key == b.key {
            return None;
        }
        tail.iter()
            .enumerate()
            .all(|(i, r)| {
                r.key
                    == if i % 2 == 0 {
                        a.key.as_str()
                    } else {
                        b.key.as_str()
                    }
            })
            .then(|| {
                format!(
                    "alternated between {} and {} for {} cycles",
                    a.tool, b.tool, self.cycles
                )
            })
    }

    fn errors_hold(&self) -> Option<String> {
        let n = self.errors;
        if n == 0 || self.history.len() < n {
            return None;
        }
        let tail = &self.history[self.history.len() - n..];
        let first = tail[0].error.as_ref()?;
        tail.iter()
            .all(|r| r.error.as_ref() == Some(first))
            .then(|| {
                format!(
                    "the same error result {n} times in a row from {}",
                    tail[0].tool
                )
            })
    }

    /// Evaluate the patterns at the end of a round.
    pub fn end_round(&mut self) -> Verdict {
        let holds = [
            (Kind::IdenticalCalls, self.identical_holds()),
            (Kind::Alternation, self.alternation_holds()),
            (Kind::IdenticalErrors, self.errors_hold()),
        ];
        let mut warn = None;
        let mut stop = None;
        for (index, (kind, detail)) in holds.into_iter().enumerate() {
            match detail {
                None => self.warned[index] = false,
                Some(detail) if self.warned[index] => {
                    stop.get_or_insert((kind, detail));
                }
                Some(detail) => {
                    self.warned[index] = true;
                    warn.get_or_insert((kind, detail));
                }
            }
        }
        if let Some((kind, detail)) = stop {
            return Verdict::Stop(kind, detail);
        }
        match warn {
            Some((kind, detail)) => {
                let text = match kind {
                    Kind::IdenticalCalls => format!(
                        "{LOOP_OBSERVATION_PREFIX} you repeated the same call ({detail}). It \
                         will not give a new result. Change the arguments, try another \
                         approach, or finish with what you have. Repeating it again stops \
                         the task."
                    ),
                    Kind::Alternation => format!(
                        "{LOOP_OBSERVATION_PREFIX} you {detail}. Repeating the same pair of \
                         calls will not give a new result. Try another approach, or finish \
                         with what you have. Continuing the pattern stops the task."
                    ),
                    Kind::IdenticalErrors => format!(
                        "{LOOP_OBSERVATION_PREFIX} you got {detail}. Read the error and \
                         change what you send, or try another approach. Getting it again \
                         stops the task."
                    ),
                };
                Verdict::Warn(kind, text)
            }
            None => Verdict::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det() -> Detector {
        Detector::new(&Loop {
            detect: true,
            ..Loop::default()
        })
    }

    /// One call per round, ok result.
    fn round(d: &mut Detector, tool: &str, args: &str) -> Verdict {
        d.observe(tool, args, "ok");
        d.end_round()
    }

    #[test]
    fn identical_calls_warn_at_five_then_stop_on_the_next_repeat() {
        let mut d = det();
        for _ in 0..4 {
            assert_eq!(round(&mut d, "read_file", "a"), Verdict::Continue);
        }
        match round(&mut d, "read_file", "a") {
            Verdict::Warn(Kind::IdenticalCalls, text) => {
                assert!(text.starts_with("Loop detected:"));
                assert!(text.contains("repeated the same call"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            round(&mut d, "read_file", "a"),
            Verdict::Stop(Kind::IdenticalCalls, _)
        ));
    }

    #[test]
    fn identical_near_miss_and_reset() {
        let mut d = det();
        for _ in 0..4 {
            round(&mut d, "read_file", "a");
        }
        assert_eq!(round(&mut d, "read_file", "b"), Verdict::Continue);
        for _ in 0..4 {
            assert_eq!(round(&mut d, "read_file", "a"), Verdict::Continue);
        }
        // Same tool, different arguments never stops a task.
        let mut d = det();
        for i in 0..100 {
            assert_eq!(
                round(&mut d, "read_file", &format!("{i}")),
                Verdict::Continue
            );
        }
    }

    #[test]
    fn warning_clears_when_the_pattern_breaks() {
        let mut d = det();
        for _ in 0..5 {
            round(&mut d, "t", "a");
        }
        assert_eq!(round(&mut d, "t", "b"), Verdict::Continue);
        for _ in 0..4 {
            assert_eq!(round(&mut d, "t", "a"), Verdict::Continue);
        }
        assert!(matches!(round(&mut d, "t", "a"), Verdict::Warn(..)));
    }

    #[test]
    fn alternation_trips_at_three_cycles() {
        let mut d = det();
        for s in ["a", "b", "a", "b", "a"] {
            assert_eq!(round(&mut d, "t", s), Verdict::Continue);
        }
        assert!(matches!(
            round(&mut d, "t", "b"),
            Verdict::Warn(Kind::Alternation, _)
        ));
        assert!(matches!(
            round(&mut d, "t", "a"),
            Verdict::Stop(Kind::Alternation, _)
        ));
    }

    #[test]
    fn alternation_near_miss() {
        let mut d = det();
        for s in ["a", "b", "a", "b", "a", "c"] {
            assert_eq!(round(&mut d, "t", s), Verdict::Continue);
        }
        let mut d = det();
        for s in ["a", "b", "a", "b"] {
            assert_eq!(round(&mut d, "t", s), Verdict::Continue);
        }
    }

    #[test]
    fn identical_errors_trip_at_three() {
        let mut d = det();
        for i in 0..2 {
            d.observe("t", &format!("{i}"), "Error: nope");
            assert_eq!(d.end_round(), Verdict::Continue);
        }
        d.observe("t", "2", "Error: nope");
        assert!(matches!(
            d.end_round(),
            Verdict::Warn(Kind::IdenticalErrors, _)
        ));
        d.observe("t", "3", "Error: nope");
        assert!(matches!(
            d.end_round(),
            Verdict::Stop(Kind::IdenticalErrors, _)
        ));
    }

    #[test]
    fn error_near_misses() {
        let mut d = det();
        d.observe("t", "1", "Error: a");
        d.observe("t", "2", "Error: a");
        d.observe("t", "3", "Error: b");
        assert_eq!(d.end_round(), Verdict::Continue);
        let mut d = det();
        d.observe("t", "1", "Error: a");
        d.observe("t", "2", "Error: a");
        d.observe("t", "3", "fine");
        assert_eq!(d.end_round(), Verdict::Continue);
    }

    #[test]
    fn typed_invalid_argument_errors_count_as_errors() {
        assert!(is_error_result(
            "invalid arguments for read_file: not valid JSON"
        ));
        assert!(is_error_result("tool_error: x"));
        assert!(!is_error_result("file contents"));
    }

    #[test]
    fn clear_breaks_every_pattern() {
        let mut d = det();
        for _ in 0..4 {
            round(&mut d, "t", "a");
        }
        d.clear();
        assert_eq!(round(&mut d, "t", "a"), Verdict::Continue);
    }

    #[test]
    fn thresholds_come_from_the_struct() {
        let mut d = Detector::new(&Loop {
            detect: true,
            identical_calls: 2,
            ..Loop::default()
        });
        assert_eq!(round(&mut d, "t", "a"), Verdict::Continue);
        assert!(matches!(round(&mut d, "t", "a"), Verdict::Warn(..)));
    }
}
