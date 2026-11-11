//! S2: no plan numbers in ids, files, pieces, labels, aliases or tags.

use regex::Regex;
use std::sync::OnceLock;

/// Number-carrying plan tokens, matched token-wise on `-`, `_`, `.`, `:` and whitespace,
/// case-insensitive. Bare words such as `check`, `phase`, `wave` or `story` are allowed, and
/// `e2e` is not a plan token (`e\d+` needs the token to end after the digits).
const PATTERN: &str = r"(?i)(^|[-_.:\s])(p\d+|phase\d+|wave\d+|e\d+|h\d+|es-?\d+|squ-?\d+|story\d+|check\d+|ar-?\d+|ax-?\d+)($|[-_.:\s])";

fn re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(PATTERN).expect("plan-number pattern"))
}

/// Returns the offending token when `name` carries a plan number.
pub fn plan_number(name: &str) -> Option<String> {
    re().captures(name).map(|c| c[2].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_plan_numbers() {
        for bad in [
            "p4-check7",
            "h1-check1-approval",
            "phase2-gate",
            "gate-wave3",
            "e10-approval",
            "es-1-thing",
            "es15",
            "squ-12",
            "story3",
            "ar-4",
            "ax7",
            "thing_P5",
        ] {
            assert!(plan_number(bad).is_some(), "{bad} should be rejected");
        }
    }

    #[test]
    fn accepts_behaviour_names() {
        for ok in [
            "agent-check-warns",
            "playbook-phase-order",
            "e2e",
            "e2e-step",
            "approval-suspends-and-resumes",
            "agent-with-builtins",
            "wave-of-requests",
            "story-mode",
            "employee-sees-release",
        ] {
            assert!(plan_number(ok).is_none(), "{ok} should be accepted");
        }
    }
}
