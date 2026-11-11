//! The vamd run-event names a scenario may expect. Bare names; vamd's types carry the
//! `vam.v1.` prefix. A test (`tests/event_names_sync.rs`) asserts every name is a
//! `"vam.v1.<name>"` literal in `crates/vam-orchestrator/src`, so the list cannot drift.

pub const PREFIX: &str = "vam.v1.";

pub const KNOWN_EVENTS: &[&str] = &[
    "card.skipped",
    "context.compacted",
    "effect.reconciled",
    "effect.recorded",
    "effect.replayed",
    "egress.decided",
    "hitl.decided",
    "hitl.expired",
    "hitl.paused",
    "host_tool.completed",
    "inbox.delivered",
    "inbox.discarded",
    "inbox.received",
    "inbox.started",
    "inference.completed",
    "mock.rule_matched",
    "mock.unmatched",
    "model.fallback",
    "model.resolved",
    "outbox.card",
    "outbox.message",
    "playbook.activated",
    "playbook.state",
    "playbook.step",
    "policy.decided",
    "run.completed",
    "run.resumed",
    "run.state",
    "secret.accessed",
    "session.booted",
    "skill.loaded",
    "tool.completed",
    "tool.failed",
    "tool.invoked",
    "web.used",
];

/// `tool.completed` for both `tool.completed` and `vam.v1.tool.completed`.
pub fn bare(name: &str) -> &str {
    name.strip_prefix(PREFIX).unwrap_or(name)
}

/// Events a pending scenario names before vamd emits them. Move a name to `KNOWN_EVENTS` in the
/// wave that emits it (the sync test reads vamd's sources for `KNOWN_EVENTS` only).
pub const PLANNED_EVENTS: &[&str] = &[];

pub fn is_known(name: &str) -> bool {
    KNOWN_EVENTS.contains(&bare(name)) || PLANNED_EVENTS.contains(&bare(name))
}

/// Up to three known names that share a prefix segment or contain the same word.
pub fn suggestions(name: &str) -> Vec<&'static str> {
    let b = bare(name);
    let head = b.split('.').next().unwrap_or(b);
    let tail = b.rsplit('.').next().unwrap_or(b);
    KNOWN_EVENTS
        .iter()
        .copied()
        .filter(|k| k.starts_with(head) || k.ends_with(tail))
        .take(3)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_forms_are_known() {
        assert!(is_known("tool.failed") && is_known("vam.v1.tool.failed"));
        assert!(!is_known("assistant.message"));
    }

    #[test]
    fn list_is_sorted_and_unique() {
        let mut v = KNOWN_EVENTS.to_vec();
        v.sort();
        v.dedup();
        assert_eq!(v, KNOWN_EVENTS);
    }
}
